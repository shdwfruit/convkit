//! PDF merge and split (`conv merge`, `conv split`), run on qpdf.
//!
//! qpdf rewrites a PDF's structure and never re-renders a page, so text,
//! links and images come through untouched. This module reads the inputs,
//! plans the qpdf calls together with the notes and warnings a person
//! should see, and runs them through a scratch folder so a failure leaves
//! nothing behind. Like the rest of convkit-core, it never prints.

pub mod range;

pub use range::{parse_range, PageRange};
