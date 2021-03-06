// Copyright (c) 2018 Pennsieve, Inc. All Rights Reserved.
use serde_derive::Deserialize;

use crate::ps::model;

/// The result of a successful login.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Deserialize)]
pub struct ApiSession {
    session_token: model::SessionToken,
    organization: String,
    expires_in: i32,
}

impl ApiSession {
    pub fn new(session_token: model::SessionToken, organization: String, expires_in: i32) -> Self {
        Self {
            session_token,
            organization,
            expires_in,
        }
    }

    pub fn session_token(&self) -> &model::SessionToken {
        &self.session_token
    }

    pub fn organization(&self) -> &String {
        &self.organization
    }

    pub fn expires_in(&self) -> i32 {
        self.expires_in
    }
}
