# Convkit Sample Document

This is a small fixture used by convkit's output-property tests
(`crates/convkit-core/tests/output_properties.rs`, Task 15). It exists
so `sample.docx` can be regenerated deterministically with `pandoc`
instead of committing an unreadable binary blob.

## Purpose

The `docx_to_pdf_produces_a_real_pdf` property test converts
`sample.docx` (built from this file) to PDF via `soffice` and checks
that the result starts with the `%PDF-` magic bytes.

## Regenerating the fixture

```
pandoc tests/fixtures/sample.md --standalone -o tests/fixtures/sample.docx
```

- A bullet point, so the DOCX has more than one paragraph style.
- Another bullet point.

1. A numbered item.
2. A second numbered item.
