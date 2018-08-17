// Copyright 2018 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![stable(feature = "stable_test_feature", since = "1.0.0")]

#![feature(staged_api)]
// Right now, stability attributes are only checked for consistency if we're
// collecting lib feature attributes, for performance. We collect lib
// feature attributes if there are non-lang feature attributes, or a crate
// that depends on the current one has non-lang feature attributes. Thus,
// we're enabling an arbitrary lib feature to force the check to kick in.
#![feature(rustc_private)]

#[stable(feature = "foo", since = "1.0.0")]
fn foo_stable_1_0_0() {}

#[stable(feature = "foo", since = "1.29.0")]
//~^ ERROR feature `foo` is declared stable since 1.29.0
fn foo_stable_1_29_0() {}

#[unstable(feature = "foo", issue = "0")]
//~^ ERROR feature `foo` is declared unstable
fn foo_unstable() {}

fn main() {}
