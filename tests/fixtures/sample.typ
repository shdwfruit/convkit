// Source of sample.pdf: `typst compile tests/fixtures/sample.typ tests/fixtures/sample.pdf`.
// Three pages, 210, 220 and 230 points wide, so tests can tell them apart
// by size, and one heading (one bookmark) on each.
#set page(width: 210pt, height: 297pt, margin: 24pt)
= Page one
First page of the convkit PDF fixture.
#pagebreak()
#set page(width: 220pt)
= Page two
Second page.
#pagebreak()
#set page(width: 230pt)
= Page three
Third page.
