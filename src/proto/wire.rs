//! Control-message encodings (`--wire`): postcard (serde, compact varints) or rkyv
//! (the original plan's format: fixed layout, validated with bytecheck on receipt).
//!
//! Each frame says which one it carries, so a reader decodes either; the client picks
//! one for the whole session and sends it in `Hello`, which is always postcard so any
//! version can read it.

use rkyv::rancor::Fallible;
use rkyv::ser::{Allocator, Writer};
use rkyv::vec::{ArchivedVec, VecResolver};
use rkyv::with::{ArchiveWith, DeserializeWith, SerializeWith};
use rkyv::Place;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};

/// Which encoding control messages use (`--wire`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize, clap::ValueEnum)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum Wire {
    #[default]
    Postcard,
    Rkyv,
}

/// rkyv "with" wrapper: an `OsString` as its raw bytes (names need not be UTF-8).
pub struct OsBytes;

impl ArchiveWith<OsString> for OsBytes {
    type Archived = ArchivedVec<u8>;
    type Resolver = VecResolver;
    fn resolve_with(field: &OsString, resolver: VecResolver, out: Place<Self::Archived>) {
        ArchivedVec::resolve_from_len(field.as_bytes().len(), resolver, out);
    }
}

impl<S: Fallible + Allocator + Writer + ?Sized> SerializeWith<OsString, S> for OsBytes {
    fn serialize_with(field: &OsString, s: &mut S) -> Result<VecResolver, S::Error> {
        ArchivedVec::serialize_from_slice(field.as_bytes(), s)
    }
}

impl<D: Fallible + ?Sized> DeserializeWith<ArchivedVec<u8>, OsString, D> for OsBytes {
    fn deserialize_with(field: &ArchivedVec<u8>, _: &mut D) -> Result<OsString, D::Error> {
        Ok(OsString::from_vec(field.as_slice().to_vec()))
    }
}
