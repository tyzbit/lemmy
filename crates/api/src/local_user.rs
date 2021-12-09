use crate::{captcha_as_wav_base64, Perform};
use actix_web::web::Data;
use anyhow::Context;
use bcrypt::verify;
use captcha::{gen, Difficulty};
use chrono::Duration;
use lemmy_api_common::{
  blocking,
  check_registration_application,
  get_local_user_view_from_jwt,
  is_admin,
  password_length_check,
  person::*,
  send_email_verification_success,
  send_password_reset_email,
  send_verification_email,
};
use lemmy_db_schema::{
  diesel_option_overwrite,
  diesel_option_overwrite_to_url,
  from_opt_str_to_opt_enum,
  naive_now,
  source::{
    comment::Comment,
    community::Community,
    email_verification::EmailVerification,
    local_user::{LocalUser, LocalUserForm},
    moderator::*,
    password_reset_request::*,
    person::*,
    person_block::{PersonBlock, PersonBlockForm},
    person_mention::*,
    post::Post,
    private_message::PrivateMessage,
    site::*,
  },
  traits::{Blockable, Crud},
  SortType,
};
use lemmy_db_views::{
  comment_report_view::CommentReportView,
  comment_view::{CommentQueryBuilder, CommentView},
  local_user_view::LocalUserView,
  post_report_view::PostReportView,
  private_message_view::PrivateMessageView,
};
use lemmy_db_views_actor::{
  community_moderator_view::CommunityModeratorView,
  person_mention_view::{PersonMentionQueryBuilder, PersonMentionView},
  person_view::PersonViewSafe,
};
use lemmy_utils::{
  claims::Claims,
  location_info,
  utils::{is_valid_display_name, is_valid_matrix_id, naive_from_unix},
  ConnectionId,
  LemmyError,
};
use lemmy_websocket::{
  messages::{CaptchaItem, SendAllMessage},
  LemmyContext,
  UserOperation,
};

#[async_trait::async_trait(?Send)]
impl Perform for Login {
  type Response = LoginResponse;

  #[tracing::instrument(skip(context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<LoginResponse, LemmyError> {
    let data: &Login = self;

    // Fetch that username / email
    let username_or_email = data.username_or_email.clone();
    let local_user_view = blocking(context.pool(), move |conn| {
      LocalUserView::find_by_email_or_name(conn, &username_or_email)
    })
    .await?
    .map_err(LemmyError::from)
    .map_err(|e| e.with_message("couldnt_find_that_username_or_email"))?;

    // Verify the password
    let valid: bool = verify(
      &data.password,
      &local_user_view.local_user.password_encrypted,
    )
    .unwrap_or(false);
    if !valid {
      return Err(LemmyError::from_message("password_incorrect"));
    }

    let site = blocking(context.pool(), Site::read_simple).await??;
    if site.require_email_verification && !local_user_view.local_user.email_verified {
      return Err(LemmyError::from_message("email_not_verified"));
    }

    check_registration_application(&site, &local_user_view, context.pool()).await?;

    // Return the jwt
    Ok(LoginResponse {
      jwt: Some(
        Claims::jwt(
          local_user_view.local_user.id.0,
          &context.secret().jwt_secret,
          &context.settings().hostname,
        )?
        .into(),
      ),
      verify_email_sent: false,
      registration_created: false,
    })
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for GetCaptcha {
  type Response = GetCaptchaResponse;

  #[tracing::instrument(skip(context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<Self::Response, LemmyError> {
    let captcha_settings = context.settings().captcha;

    if !captcha_settings.enabled {
      return Ok(GetCaptchaResponse { ok: None });
    }

    let captcha = match captcha_settings.difficulty.as_str() {
      "easy" => gen(Difficulty::Easy),
      "medium" => gen(Difficulty::Medium),
      "hard" => gen(Difficulty::Hard),
      _ => gen(Difficulty::Medium),
    };

    let answer = captcha.chars_as_string();

    let png = captcha.as_base64().expect("failed to generate captcha");

    let uuid = uuid::Uuid::new_v4().to_string();

    let wav = captcha_as_wav_base64(&captcha);

    let captcha_item = CaptchaItem {
      answer,
      uuid: uuid.to_owned(),
      expires: naive_now() + Duration::minutes(10), // expires in 10 minutes
    };

    // Stores the captcha item on the queue
    context.chat_server().do_send(captcha_item);

    Ok(GetCaptchaResponse {
      ok: Some(CaptchaResponse { png, wav, uuid }),
    })
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for SaveUserSettings {
  type Response = LoginResponse;

  #[tracing::instrument(skip(context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<LoginResponse, LemmyError> {
    let data: &SaveUserSettings = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;

    let avatar = diesel_option_overwrite_to_url(&data.avatar)?;
    let banner = diesel_option_overwrite_to_url(&data.banner)?;
    let bio = diesel_option_overwrite(&data.bio);
    let display_name = diesel_option_overwrite(&data.display_name);
    let matrix_user_id = diesel_option_overwrite(&data.matrix_user_id);
    let bot_account = data.bot_account;
    let email_deref = data.email.as_deref().map(|e| e.to_owned());

    let email = if let Some(email) = &email_deref {
      let site = blocking(context.pool(), Site::read_simple).await??;
      if site.require_email_verification {
        if let Some(previous_email) = local_user_view.local_user.email {
          // Only send the verification email if there was an email change
          if previous_email.ne(email) {
            send_verification_email(
              local_user_view.local_user.id,
              email,
              &local_user_view.person.name,
              context.pool(),
              &context.settings(),
            )
            .await?;
          }
        }
        // Fine to return None here, because the actual email is also in the email_verification
        // table, and gets set in the function below.
        None
      } else {
        diesel_option_overwrite(&email_deref)
      }
    } else {
      None
    };

    if let Some(Some(bio)) = &bio {
      if bio.chars().count() > 300 {
        return Err(LemmyError::from_message("bio_length_overflow"));
      }
    }

    if let Some(Some(display_name)) = &display_name {
      if !is_valid_display_name(
        display_name.trim(),
        context.settings().actor_name_max_length,
      ) {
        return Err(LemmyError::from_message("invalid_username"));
      }
    }

    if let Some(Some(matrix_user_id)) = &matrix_user_id {
      if !is_valid_matrix_id(matrix_user_id) {
        return Err(LemmyError::from_message("invalid_matrix_id"));
      }
    }

    let local_user_id = local_user_view.local_user.id;
    let person_id = local_user_view.person.id;
    let default_listing_type = data.default_listing_type;
    let default_sort_type = data.default_sort_type;
    let password_encrypted = local_user_view.local_user.password_encrypted;
    let public_key = local_user_view.person.public_key;

    let person_form = PersonForm {
      name: local_user_view.person.name,
      avatar,
      banner,
      inbox_url: None,
      display_name,
      published: None,
      updated: Some(naive_now()),
      banned: None,
      deleted: None,
      actor_id: None,
      bio,
      local: None,
      admin: None,
      private_key: None,
      public_key,
      last_refreshed_at: None,
      shared_inbox_url: None,
      matrix_user_id,
      bot_account,
    };

    blocking(context.pool(), move |conn| {
      Person::update(conn, person_id, &person_form)
    })
    .await?
    .map_err(LemmyError::from)
    .map_err(|e| e.with_message("user_already_exists"))?;

    let local_user_form = LocalUserForm {
      person_id: Some(person_id),
      email,
      password_encrypted: Some(password_encrypted),
      show_nsfw: data.show_nsfw,
      show_bot_accounts: data.show_bot_accounts,
      show_scores: data.show_scores,
      theme: data.theme.to_owned(),
      default_sort_type,
      default_listing_type,
      lang: data.lang.to_owned(),
      show_avatars: data.show_avatars,
      show_read_posts: data.show_read_posts,
      show_new_post_notifs: data.show_new_post_notifs,
      send_notifications_to_email: data.send_notifications_to_email,
      email_verified: None,
      accepted_application: None,
    };

    let local_user_res = blocking(context.pool(), move |conn| {
      LocalUser::update(conn, local_user_id, &local_user_form)
    })
    .await?;
    let updated_local_user = match local_user_res {
      Ok(u) => u,
      Err(e) => {
        let err_type = if e.to_string()
          == "duplicate key value violates unique constraint \"local_user_email_key\""
        {
          "email_already_exists"
        } else {
          "user_already_exists"
        };

        return Err(LemmyError::from(e).with_message(err_type));
      }
    };

    // Return the jwt
    Ok(LoginResponse {
      jwt: Some(
        Claims::jwt(
          updated_local_user.id.0,
          &context.secret().jwt_secret,
          &context.settings().hostname,
        )?
        .into(),
      ),
      verify_email_sent: false,
      registration_created: false,
    })
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for ChangePassword {
  type Response = LoginResponse;

  #[tracing::instrument(skip(self, context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<LoginResponse, LemmyError> {
    let data: &ChangePassword = self;
    let local_user_view =
      get_local_user_view_from_jwt(data.auth.as_ref(), context.pool(), context.secret()).await?;

    password_length_check(&data.new_password)?;

    // Make sure passwords match
    if data.new_password != data.new_password_verify {
      return Err(LemmyError::from_message("passwords_dont_match"));
    }

    // Check the old password
    let valid: bool = verify(
      &data.old_password,
      &local_user_view.local_user.password_encrypted,
    )
    .unwrap_or(false);
    if !valid {
      return Err(LemmyError::from_message("password_incorrect"));
    }

    let local_user_id = local_user_view.local_user.id;
    let new_password = data.new_password.to_owned();
    let updated_local_user = blocking(context.pool(), move |conn| {
      LocalUser::update_password(conn, local_user_id, &new_password)
    })
    .await??;

    // Return the jwt
    Ok(LoginResponse {
      jwt: Some(
        Claims::jwt(
          updated_local_user.id.0,
          &context.secret().jwt_secret,
          &context.settings().hostname,
        )?
        .into(),
      ),
      verify_email_sent: false,
      registration_created: false,
    })
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for AddAdmin {
  type Response = AddAdminResponse;

  #[tracing::instrument(skip(context, websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    websocket_id: Option<ConnectionId>,
  ) -> Result<AddAdminResponse, LemmyError> {
    let data: &AddAdmin = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;

    // Make sure user is an admin
    is_admin(&local_user_view)?;

    let added = data.added;
    let added_person_id = data.person_id;
    let added_admin = blocking(context.pool(), move |conn| {
      Person::add_admin(conn, added_person_id, added)
    })
    .await?
    .map_err(LemmyError::from)
    .map_err(|e| e.with_message("couldnt_update_user"))?;

    // Mod tables
    let form = ModAddForm {
      mod_person_id: local_user_view.person.id,
      other_person_id: added_admin.id,
      removed: Some(!data.added),
    };

    blocking(context.pool(), move |conn| ModAdd::create(conn, &form)).await??;

    let site_creator_id = blocking(context.pool(), move |conn| {
      Site::read(conn, 1).map(|s| s.creator_id)
    })
    .await??;

    let mut admins = blocking(context.pool(), PersonViewSafe::admins).await??;
    let creator_index = admins
      .iter()
      .position(|r| r.person.id == site_creator_id)
      .context(location_info!())?;
    let creator_person = admins.remove(creator_index);
    admins.insert(0, creator_person);

    let res = AddAdminResponse { admins };

    context.chat_server().do_send(SendAllMessage {
      op: UserOperation::AddAdmin,
      response: res.clone(),
      websocket_id,
    });

    Ok(res)
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for BanPerson {
  type Response = BanPersonResponse;

  #[tracing::instrument(skip(context, websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    websocket_id: Option<ConnectionId>,
  ) -> Result<BanPersonResponse, LemmyError> {
    let data: &BanPerson = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;

    // Make sure user is an admin
    is_admin(&local_user_view)?;

    let ban = data.ban;
    let banned_person_id = data.person_id;
    let ban_person = move |conn: &'_ _| Person::ban_person(conn, banned_person_id, ban);
    blocking(context.pool(), ban_person)
      .await?
      .map_err(LemmyError::from)
      .map_err(|e| e.with_message("couldnt_update_user"))?;

    // Remove their data if that's desired
    if data.remove_data.unwrap_or(false) {
      // Posts
      blocking(context.pool(), move |conn: &'_ _| {
        Post::update_removed_for_creator(conn, banned_person_id, None, true)
      })
      .await??;

      // Communities
      // Remove all communities where they're the top mod
      // for now, remove the communities manually
      let first_mod_communities = blocking(context.pool(), move |conn: &'_ _| {
        CommunityModeratorView::get_community_first_mods(conn)
      })
      .await??;

      // Filter to only this banned users top communities
      let banned_user_first_communities: Vec<CommunityModeratorView> = first_mod_communities
        .into_iter()
        .filter(|fmc| fmc.moderator.id == banned_person_id)
        .collect();

      for first_mod_community in banned_user_first_communities {
        blocking(context.pool(), move |conn: &'_ _| {
          Community::update_removed(conn, first_mod_community.community.id, true)
        })
        .await??;
      }

      // Comments
      blocking(context.pool(), move |conn: &'_ _| {
        Comment::update_removed_for_creator(conn, banned_person_id, true)
      })
      .await??;
    }

    // Mod tables
    let expires = data.expires.map(naive_from_unix);

    let form = ModBanForm {
      mod_person_id: local_user_view.person.id,
      other_person_id: data.person_id,
      reason: data.reason.to_owned(),
      banned: Some(data.ban),
      expires,
    };

    blocking(context.pool(), move |conn| ModBan::create(conn, &form)).await??;

    let person_id = data.person_id;
    let person_view = blocking(context.pool(), move |conn| {
      PersonViewSafe::read(conn, person_id)
    })
    .await??;

    let res = BanPersonResponse {
      person_view,
      banned: data.ban,
    };

    context.chat_server().do_send(SendAllMessage {
      op: UserOperation::BanPerson,
      response: res.clone(),
      websocket_id,
    });

    Ok(res)
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for BlockPerson {
  type Response = BlockPersonResponse;

  #[tracing::instrument(skip(context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<BlockPersonResponse, LemmyError> {
    let data: &BlockPerson = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;

    let target_id = data.person_id;
    let person_id = local_user_view.person.id;

    // Don't let a person block themselves
    if target_id == person_id {
      return Err(LemmyError::from_message("cant_block_yourself"));
    }

    let person_block_form = PersonBlockForm {
      person_id,
      target_id,
    };

    if data.block {
      let block = move |conn: &'_ _| PersonBlock::block(conn, &person_block_form);
      blocking(context.pool(), block)
        .await?
        .map_err(LemmyError::from)
        .map_err(|e| e.with_message("person_block_already_exists"))?;
    } else {
      let unblock = move |conn: &'_ _| PersonBlock::unblock(conn, &person_block_form);
      blocking(context.pool(), unblock)
        .await?
        .map_err(LemmyError::from)
        .map_err(|e| e.with_message("person_block_already_exists"))?;
    }

    // TODO does any federated stuff need to be done here?

    let person_view = blocking(context.pool(), move |conn| {
      PersonViewSafe::read(conn, target_id)
    })
    .await??;

    let res = BlockPersonResponse {
      person_view,
      blocked: data.block,
    };

    Ok(res)
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for GetReplies {
  type Response = GetRepliesResponse;

  #[tracing::instrument(skip(context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<GetRepliesResponse, LemmyError> {
    let data: &GetReplies = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;

    let sort: Option<SortType> = from_opt_str_to_opt_enum(&data.sort);

    let page = data.page;
    let limit = data.limit;
    let unread_only = data.unread_only;
    let person_id = local_user_view.person.id;
    let show_bot_accounts = local_user_view.local_user.show_bot_accounts;

    let replies = blocking(context.pool(), move |conn| {
      CommentQueryBuilder::create(conn)
        .sort(sort)
        .unread_only(unread_only)
        .recipient_id(person_id)
        .show_bot_accounts(show_bot_accounts)
        .my_person_id(person_id)
        .page(page)
        .limit(limit)
        .list()
    })
    .await??;

    Ok(GetRepliesResponse { replies })
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for GetPersonMentions {
  type Response = GetPersonMentionsResponse;

  #[tracing::instrument(skip(context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<GetPersonMentionsResponse, LemmyError> {
    let data: &GetPersonMentions = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;

    let sort: Option<SortType> = from_opt_str_to_opt_enum(&data.sort);

    let page = data.page;
    let limit = data.limit;
    let unread_only = data.unread_only;
    let person_id = local_user_view.person.id;
    let mentions = blocking(context.pool(), move |conn| {
      PersonMentionQueryBuilder::create(conn)
        .recipient_id(person_id)
        .my_person_id(person_id)
        .sort(sort)
        .unread_only(unread_only)
        .page(page)
        .limit(limit)
        .list()
    })
    .await??;

    Ok(GetPersonMentionsResponse { mentions })
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for MarkPersonMentionAsRead {
  type Response = PersonMentionResponse;

  #[tracing::instrument(skip(context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<PersonMentionResponse, LemmyError> {
    let data: &MarkPersonMentionAsRead = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;

    let person_mention_id = data.person_mention_id;
    let read_person_mention = blocking(context.pool(), move |conn| {
      PersonMention::read(conn, person_mention_id)
    })
    .await??;

    if local_user_view.person.id != read_person_mention.recipient_id {
      return Err(LemmyError::from_message("couldnt_update_comment"));
    }

    let person_mention_id = read_person_mention.id;
    let read = data.read;
    let update_mention =
      move |conn: &'_ _| PersonMention::update_read(conn, person_mention_id, read);
    blocking(context.pool(), update_mention)
      .await?
      .map_err(LemmyError::from)
      .map_err(|e| e.with_message("couldnt_update_comment"))?;

    let person_mention_id = read_person_mention.id;
    let person_id = local_user_view.person.id;
    let person_mention_view = blocking(context.pool(), move |conn| {
      PersonMentionView::read(conn, person_mention_id, Some(person_id))
    })
    .await??;

    Ok(PersonMentionResponse {
      person_mention_view,
    })
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for MarkAllAsRead {
  type Response = GetRepliesResponse;

  #[tracing::instrument(skip(context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<GetRepliesResponse, LemmyError> {
    let data: &MarkAllAsRead = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;

    let person_id = local_user_view.person.id;
    let replies = blocking(context.pool(), move |conn| {
      CommentQueryBuilder::create(conn)
        .my_person_id(person_id)
        .recipient_id(person_id)
        .unread_only(true)
        .page(1)
        .limit(999)
        .list()
    })
    .await??;

    // TODO: this should probably be a bulk operation
    // Not easy to do as a bulk operation,
    // because recipient_id isn't in the comment table
    for comment_view in &replies {
      let reply_id = comment_view.comment.id;
      let mark_as_read = move |conn: &'_ _| Comment::update_read(conn, reply_id, true);
      blocking(context.pool(), mark_as_read)
        .await?
        .map_err(LemmyError::from)
        .map_err(|e| e.with_message("couldnt_update_comment"))?;
    }

    // Mark all user mentions as read
    let update_person_mentions =
      move |conn: &'_ _| PersonMention::mark_all_as_read(conn, person_id);
    blocking(context.pool(), update_person_mentions)
      .await?
      .map_err(LemmyError::from)
      .map_err(|e| e.with_message("couldnt_update_comment"))?;

    // Mark all private_messages as read
    let update_pm = move |conn: &'_ _| PrivateMessage::mark_all_as_read(conn, person_id);
    blocking(context.pool(), update_pm)
      .await?
      .map_err(LemmyError::from)
      .map_err(|e| e.with_message("couldnt_update_private_message"))?;

    Ok(GetRepliesResponse { replies: vec![] })
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for PasswordReset {
  type Response = PasswordResetResponse;

  #[tracing::instrument(skip(self, context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<PasswordResetResponse, LemmyError> {
    let data: &PasswordReset = self;

    // Fetch that email
    let email = data.email.clone();
    let local_user_view = blocking(context.pool(), move |conn| {
      LocalUserView::find_by_email(conn, &email)
    })
    .await?
    .map_err(LemmyError::from)
    .map_err(|e| e.with_message("couldnt_find_that_username_or_email"))?;

    // Email the pure token to the user.
    send_password_reset_email(&local_user_view, context.pool(), &context.settings()).await?;
    Ok(PasswordResetResponse {})
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for PasswordChange {
  type Response = LoginResponse;

  #[tracing::instrument(skip(self, context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<LoginResponse, LemmyError> {
    let data: &PasswordChange = self;

    // Fetch the user_id from the token
    let token = data.token.clone();
    let local_user_id = blocking(context.pool(), move |conn| {
      PasswordResetRequest::read_from_token(conn, &token).map(|p| p.local_user_id)
    })
    .await??;

    password_length_check(&data.password)?;

    // Make sure passwords match
    if data.password != data.password_verify {
      return Err(LemmyError::from_message("passwords_dont_match"));
    }

    // Update the user with the new password
    let password = data.password.clone();
    let updated_local_user = blocking(context.pool(), move |conn| {
      LocalUser::update_password(conn, local_user_id, &password)
    })
    .await?
    .map_err(LemmyError::from)
    .map_err(|e| e.with_message("couldnt_update_user"))?;

    // Return the jwt
    Ok(LoginResponse {
      jwt: Some(
        Claims::jwt(
          updated_local_user.id.0,
          &context.secret().jwt_secret,
          &context.settings().hostname,
        )?
        .into(),
      ),
      verify_email_sent: false,
      registration_created: false,
    })
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for GetReportCount {
  type Response = GetReportCountResponse;

  #[tracing::instrument(skip(context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<GetReportCountResponse, LemmyError> {
    let data: &GetReportCount = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;

    let person_id = local_user_view.person.id;
    let admin = local_user_view.person.admin;
    let community_id = data.community_id;

    let comment_reports = blocking(context.pool(), move |conn| {
      CommentReportView::get_report_count(conn, person_id, admin, community_id)
    })
    .await??;

    let post_reports = blocking(context.pool(), move |conn| {
      PostReportView::get_report_count(conn, person_id, admin, community_id)
    })
    .await??;

    let res = GetReportCountResponse {
      community_id,
      comment_reports,
      post_reports,
    };

    Ok(res)
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for GetUnreadCount {
  type Response = GetUnreadCountResponse;

  #[tracing::instrument(skip(context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<Self::Response, LemmyError> {
    let data = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;

    let person_id = local_user_view.person.id;

    let replies = blocking(context.pool(), move |conn| {
      CommentView::get_unread_replies(conn, person_id)
    })
    .await??;

    let mentions = blocking(context.pool(), move |conn| {
      PersonMentionView::get_unread_mentions(conn, person_id)
    })
    .await??;

    let private_messages = blocking(context.pool(), move |conn| {
      PrivateMessageView::get_unread_messages(conn, person_id)
    })
    .await??;

    let res = Self::Response {
      replies,
      mentions,
      private_messages,
    };

    Ok(res)
  }
}

#[async_trait::async_trait(?Send)]
impl Perform for VerifyEmail {
  type Response = LoginResponse;

  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<usize>,
  ) -> Result<Self::Response, LemmyError> {
    let token = self.token.clone();
    let verification = blocking(context.pool(), move |conn| {
      EmailVerification::read_for_token(conn, &token)
    })
    .await?
    .map_err(LemmyError::from)
    .map_err(|e| e.with_message("token_not_found"))?;

    let form = LocalUserForm {
      // necessary in case this is a new signup
      email_verified: Some(true),
      // necessary in case email of an existing user was changed
      email: Some(Some(verification.email)),
      ..LocalUserForm::default()
    };
    let local_user_id = verification.local_user_id;
    blocking(context.pool(), move |conn| {
      LocalUser::update(conn, local_user_id, &form)
    })
    .await??;

    let local_user_view = blocking(context.pool(), move |conn| {
      LocalUserView::read(conn, local_user_id)
    })
    .await??;

    send_email_verification_success(&local_user_view, &context.settings())?;

    blocking(context.pool(), move |conn| {
      EmailVerification::delete_old_tokens_for_local_user(conn, local_user_id)
    })
    .await??;

    let site = blocking(context.pool(), Site::read_simple).await??;
    check_registration_application(&site, &local_user_view, context.pool()).await?;

    // Return the jwt
    Ok(LoginResponse {
      jwt: Some(
        Claims::jwt(
          local_user_view.local_user.id.0,
          &context.secret().jwt_secret,
          &context.settings().hostname,
        )?
        .into(),
      ),
      verify_email_sent: false,
      registration_created: false,
    })
  }
}
