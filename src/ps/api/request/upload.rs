// Copyright (c) 2018 Pennsieve, Inc. All Rights Reserved.
use serde_derive::Serialize;

use crate::ps::model::S3File;

/// A preview of files to be uploaded to the Pennsieve platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadPreview {
    files: Vec<S3File>,
}

impl UploadPreview {
    #[allow(dead_code)]
    pub fn new(files: &[S3File]) -> Self {
        Self {
            files: files.to_owned(),
        }
    }
}
