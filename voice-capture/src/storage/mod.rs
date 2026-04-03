//! Session data storage: S3 uploads and metadata serialization.

pub mod bundle;
pub mod s3;

pub use bundle::pseudonymize;
pub use s3::S3Uploader;
