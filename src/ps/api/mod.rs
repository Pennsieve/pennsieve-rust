// Copyright (c) 2018 Pennsieve, Inc. All Rights Reserved.

//! The Pennsieve platform API.

mod client;
pub mod request;
pub mod response;

use std::borrow::Borrow;

pub use self::client::progress::{ProgressCallback, ProgressUpdate};

pub use self::client::Pennsieve;

/// Objects with a Pennsieve identifier implement this trait.
pub trait PSId {
    type Id: Clone + PartialEq + Into<String>;

    /// Gets the Pennsieve ID.
    fn id(&self) -> &Self::Id;
}

/// Objects with a Pennsieve-designated name implement this trait.
pub trait PSName {
    /// Gets the Pennsieve-designated name.
    fn name(&self) -> &String;
}

/// Objects that contain child objects implement this trait.
pub trait PSChildren {
    type Child: PSId + PSName;

    /// Get the child objects contained in the parent.
    fn children(&self) -> Option<&Vec<Self::Child>>;

    fn get_child_by_id<I>(&self, child_id: I) -> Option<&Self::Child>
    where
        I: Into<<<Self as PSChildren>::Child as PSId>::Id>,
    {
        let child_id: <<Self as PSChildren>::Child as PSId>::Id = child_id.into();
        let children: Option<&Vec<Self::Child>> = self.children();
        children.and_then(|children: &Vec<Self::Child>| {
            children.iter().map(|child| child.borrow()).find(|child| {
                let id: <<Self as PSChildren>::Child as PSId>::Id = child.id().clone();
                id == child_id
            })
        })
    }

    fn get_child_by_name<N: Into<String>>(&self, name: N) -> Option<&Self::Child> {
        let child_name: String = name.into();
        let children: Option<&Vec<Self::Child>> = self.children();
        children.and_then(|children: &Vec<Self::Child>| {
            children
                .iter()
                .map(|child| child.borrow())
                .find(|child| child.name() == &child_name)
        })
    }
}
