// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! User-related utilities.

use nix::unistd::geteuid;

pub struct User;

impl User {
    /// Check if the current user is root.
    ///
    /// # Returns
    ///
    /// `true` if the current user is root, `false` otherwise.
    pub fn is_root() -> bool {
        geteuid().is_root()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_root() {
        let is_root = User::is_root();
        println!("Current user is root: {}", is_root);
    }
}
