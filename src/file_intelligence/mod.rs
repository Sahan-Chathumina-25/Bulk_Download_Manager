//! Lightweight, local file intelligence for archive discovery.
//!
//! The engine is deterministic and fully offline for classification: it
//! combines filename analysis, HTTP metadata (when the user opts in to a
//! bounded probe), the existing ad/tracker filter and multipart naming
//! analysis into a transparent score with a plain-English evidence trail.
//!
//! No external AI API is contacted, no model is downloaded, and no URL or
//! history data leaves the machine. A future local model can be added behind
//! [`classifier::Classifier`]; it can never override URL safety, SSRF,
//! redirect validation or signature verification.

pub mod archive;
pub mod classifier;
pub mod engine;
pub mod metadata;
pub mod multipart;
pub mod report;
pub mod scoring;
