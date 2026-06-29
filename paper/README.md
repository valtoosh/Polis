# polis paper

This directory holds the LaTeX source and a Markdown companion for *polis: A
Programmable Polity for Self-Sustaining Autonomous AI Agents*.

```
paper/
├── polis.tex      # LaTeX source (double-column, IEEEtran conference, 10pt)
├── polis.md       # Markdown companion (slightly compressed, mermaid diagrams)
├── figures/       # Reserved for external PDFs. All current figures are inline TikZ.
└── README.md
```

## Format

`polis.tex` is a **double-column conference manuscript** using
`\documentclass[10pt,conference,letterpaper]{IEEEtran}`. The layout is
intended to be submission-ready for IEEE / USENIX / Financial
Cryptography style venues. All figures are inline TikZ and all
algorithm pseudocode lives in `algorithm` / `algpseudocode` boxes —
there is no dependency on external graphics.

## Compiling the LaTeX

Two `pdflatex` passes (the second resolves cross-references):

```bash
pdflatex polis.tex
pdflatex polis.tex
```

If you prefer `latexmk`:

```bash
latexmk -pdf polis.tex
```

`latexmk -c` cleans intermediate files.

### Packages used

Everything below ships with a standard TeX Live install (typically the
`texlive-latex-extra`, `texlive-science`, and `texlive-pictures`
bundles):

- `IEEEtran` (document class)
- `inputenc`, `fontenc`, `microtype`
- `amsmath`, `amssymb`, `amsthm`
- `booktabs`, `tabularx`, `array`
- `xcolor`
- `listings`
- `algorithm`, `algpseudocode`
- `tikz` (libraries: `arrows.meta`, `positioning`, `calc`,
  `shapes.geometric`, `fit`, `chains`, `backgrounds`,
  `decorations.pathreplacing`)
- `balance` (column-balancing on the last page)
- `hyperref` (loaded last, with `hidelinks`)

No exotic packages, no BibTeX run required — references live inline
in a `thebibliography` environment for portability.

## Figures, tables, and algorithms

The paper draws all figures inline with TikZ. There are no external
PDF dependencies. Figures of note:

- `fig:arch` — full-width four-layer architecture (CPI and dependency edges)
- `fig:hf` — Treasury health-factor state machine
- `fig:x402seq` — full-width x402 end-to-end sequence diagram
- `fig:waterfall` — repayment waterfall (production design)
- `fig:badabsorb` — bad-debt absorption order

Tables:

- `tab:related` — full-width related-work comparison
- `tab:kya` — KYA (Know-Your-Agent) tier table
- `tab:bcs` — BCS components and weights
- `tab:credit-ladder` — credit-rep coupling
- `tab:demo-params` — demo parameters
- `tab:pass` — pass criteria
- `tab:deploy` — deployed program IDs (devnet)

Algorithm boxes:

- `alg:guard` — eight-layer outbound wallet guard
- `alg:borrow` — credit borrow
- `alg:repay` — 2-instruction atomic repayment
- `alg:default` — `mark_default` (slash + LP haircut)
- `alg:vouch` — vouch with lazy yield + slash generation
- `alg:bcs` — daily BCS oracle update
- `alg:agent` — reference agent main loop
- `alg:score`, `alg:distribute` — Appendix A

## Markdown companion

`polis.md` is meant for GitHub rendering, Notion import, and quick
review during writing. It uses mermaid for the architecture diagram
and the 14-day Gantt chart. The content is intentionally slightly
compressed relative to the LaTeX: where the LaTeX has a full proof
environment, the Markdown has a one-paragraph sketch.

The two files should not diverge in their load-bearing claims. When
you edit one, propagate the change to the other.

## Placeholders versus real content

The skeleton contains real text for the spine of the paper (abstract,
intro, wallet-history fallacy, threat model, related work, all five
implemented primitives, formal properties). Placeholder markers only
appear where demo data is genuinely required:

- `§15.4 Results` — needs the 14-day audit output.
- `Theorem 5 (P-No-Human)` — concrete transaction counts.
- Each proof sketch carries an explicit `[Mechanised verification
  deferred]` marker.
