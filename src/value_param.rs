// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use moveit::{CopyNew, New};

use std::{mem::MaybeUninit, pin::Pin};

/// A trait which is used to receive any C++ parameter passed by value.
/// This trait is implemented both for references `&T` and for `T` itself,
/// subject to the presence or absence of suitable copy and move constructors.
/// This allows you to pass in parameters by copy (as is ergonomic and normal
/// in C++) retaining the original parameter; or by move semantics thus
/// destroying the object you're passing in. Simply use a reference if you want
/// copy semantics, or the item itself if you want move semantics.
/// It is not recommended that you implement this trait. If you want to do
/// something out of the ordinary here, instead implement [`New`] - there's
/// a blanket implementation which will implement this trait for anything
/// that implements [`New`].
pub trait ValueParam<T> {
    unsafe fn new(self, this: Pin<&mut MaybeUninit<T>>);
}

// impl<T> ValueParam<T> for T
// where
//     T: New<Output = T>,
// {
//     type Output = T;
//     unsafe fn new(self, this: Pin<&mut MaybeUninit<Self::Output>>) {
//         self.new(this);
//     }
// }

impl<T> ValueParam<T> for &T
where
    T: CopyNew,
{
    unsafe fn new(self, this: Pin<&mut MaybeUninit<T>>) {
        crate::moveit::new::copy(self).new(this);
    }
}

// impl<'a, T> ValueParam<T> for T
// where
//     T: 'a + MoveNew,
//     &'a mut T: DerefMove,
//     &'a mut T: Deref<Target = T>
// {
//     unsafe fn new(mut self, this: Pin<&mut MaybeUninit<T>>) {
//         let pin = std::pin::Pin::new_unchecked(&mut self);
//         let cons = crate::moveit::new::mov(pin);
//         cons.new(this);
//     }
// }
