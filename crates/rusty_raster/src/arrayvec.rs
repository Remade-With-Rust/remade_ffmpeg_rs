// Copyright 2026 Mata Network
//
// Licensed under the Apache License, Version 2.0.

//! A minimal stand-in for the `arrayvec` crate's `ArrayVec`.
//!
//! tiny-skia keeps pipeline stages and clipped edges in a fixed-capacity
//! `ArrayVec` that panics when the capacity is exceeded. This one is a `Vec`
//! reserved to the capacity up front: the bound is still documented and
//! debug-asserted, but an unexpected overflow grows the vector instead of
//! panicking.

use alloc::vec::Vec;

#[derive(Clone, Debug)]
pub struct ArrayVec<T, const N: usize>(Vec<T>);

impl<T, const N: usize> ArrayVec<T, N> {
    #[inline]
    pub fn new() -> Self {
        ArrayVec(Vec::with_capacity(N))
    }

    #[inline]
    pub fn push(&mut self, v: T) {
        debug_assert!(self.0.len() < N, "ArrayVec capacity {N} exceeded");
        self.0.push(v);
    }
}

impl<T, const N: usize> Default for ArrayVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> core::ops::Deref for ArrayVec<T, N> {
    type Target = Vec<T>;
    #[inline]
    fn deref(&self) -> &Vec<T> {
        &self.0
    }
}

impl<T, const N: usize> core::ops::DerefMut for ArrayVec<T, N> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Vec<T> {
        &mut self.0
    }
}

impl<T, const N: usize> FromIterator<T> for ArrayVec<T, N> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut v = Self::new();
        for x in iter {
            v.push(x);
        }
        v
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a ArrayVec<T, N> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a mut ArrayVec<T, N> {
    type Item = &'a mut T;
    type IntoIter = core::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter_mut()
    }
}

impl<T, const N: usize> IntoIterator for ArrayVec<T, N> {
    type Item = T;
    type IntoIter = alloc::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
