# ADR-002: E-invoicing standard and provider scope (#148)

Status: **accepted** (decision record; implementation is follow-up tickets)
Applies to: Phase 4 invoicing (#8/#113/#116) and any future delivery provider

## Context

Peppol-style B2B e-invoicing is becoming mandatory across the EU on staggered
timelines. Grounded as of October 2026:

| Jurisdiction | State | Format / channel | Notes |
| --- | --- | --- | --- |
| Belgium | **live** (1 Jan 2026) | Peppol BIS Billing 3.0 (UBL 2.1, EN 16931) | domestic VAT-registered B2B; e-reporting via Peppol 5-corner planned 2028 |
| France | receive-mandatory 1 Sep 2026; issuing phases Sep 2026 (large) → Sep 2027 (SMEs) | Factur-X / UBL / CII via certified PDPs (`Plateformes de Dématérialisation Partenaires`) | PDP registration/certification is per-country |
| Germany | receiving since 2025; issuing 2027–2028 | XRechnung or ZUGFeRD 2.x (EN 16931) | Peppol accepted via the interoperability point |
| Italy / Poland / Romania | national systems | SDI / KSeF / e-Factura | clearance models (state platform in the loop), not Peppol |
| EU-wide | ViDA digital reporting target ~2030 | cross-border | interoperability pressure toward EN 16931 |

TucanoTime's market is a *small team billing its own tracked work* — typically a
supplier INTO these jurisdictions rather than the regulated domestic taxpayer.
The obligation that actually bites us is: *"must be able to issue a structured
EN 16931 e-invoice that the buyer's network can receive."*

## Decision

1. **Semantic core: EN 16931.** Every generated artefact maps from one internal
   semantic model (the existing snapshot-locked `Invoice` + #138 company
   identity + #139 customer tax identifiers). No jurisdiction-specific dialects
   in the domain layer.
2. **v1 formats: two, both file-level, zero network.**
   - **Peppol BIS Billing 3.0 (UBL 2.1) XML** — a pure, deterministic writer
     beside the PDF renderer (#113's `doc_for` pattern: `einvoices/<id>.xml`,
     written at issue, sha256-hinted, downloadable). Covers BE/DE/NL/Nordics
     and is the interop lingua franca.
   - **Factur-X (ZUGFeRD 2.x) hybrid PDF** — PDF/A-3 embedding of the CII XML:
     the existing #113 visual PDF gains an embedded XML attachment. One file
     that satisfies French/German "PDF + structured data" expectations without
     any transport integration.
3. **Transport: provider adapter seam only — we never become an Access Point.**
   Delivery rides the #18 seam discipline like payments/accounting: an
   `EInvoiceSender` port with adapters for a commercial access point / PDP
   (candidates: Storecove, ecosio, Recomly — REST APIs, sandbox environments).
   Disabled unless `einvoice.provider` + vault credentials (`einvoice.*`, #77)
   exist, mirroring the SMTP/`DisabledEmailSender` pattern. CI records sends,
   never performs them.
4. **Explicitly out of scope for v1:** state-clearance models (Italy SDI,
   Poland KSeF, Romania e-Factura), French PDP certification/registration as a
   PDP itself, e-reporting/ViDA digital tax submission, and legal-archive
   retention guarantees beyond the existing #94 backup story. These need a
   certified intermediary per country — a provider decision, not ours.

## Consequences

**Data model additions (all `#[serde(default)]`, backward compatible):**
- Customer (via #139): `tax_id`, `peppol_participant_id` (scheme + id pair),
  postal address (already in #139).
- Org identity (via #138): legal name, address, VAT/EORI id, Peppol participant
  id, default payment means/terms.
- Invoice: the #143 line model must carry a **tax breakdown** (category +
  percent per line — EN 16931 rejects "no tax information"; current model has
  no taxes at all) and `payment_means` (UNTDID 4461 code, default from
  `pay_30`-style terms). These fields land with #143 so the e-invoice model
  never forks.

**API/OpenAPI (future implementation ticket):** `POST /invoices/{id}/einvoice`
(generate + archive `ubl` | `factur-x`, admin tier like `/pdf`),
`GET /invoices/{id}/einvoice?format=`; delivery: `POST /invoices/{id}/einvoice/send`
through the provider port; invoice gains an `einvoice` hint object mirroring
the `pdf` hint. Failure/retry/audit states copy the accounting-sync shape
(#33): record per (invoice, provider, kind), `SyncStatus`-style lifecycle,
background retry with attempt caps, `einvoice_*` audit events.

**Determinism & testing:** the XML writers are pure like `render_invoice_pdf`
(same Doc-in ⇒ bytes-out), so the archive hash story (#113) transfers verbatim;
tests validate output against the BIS 3.0 / Factur-X golden fixtures and run
`xmllint`-style well-formedness + structural asserts in-process (a hand-rolled
schematron pass is out; CI dependency: none — plain string/XML assertions with
a small dev-only XML reader if needed).

**Why not bigger bets:** doing nothing (PDF-only) fails the live-Belgium
reality for our customers' EU buyers; doing KSeF/SDI nationally means
certification we cannot obtain from a file-based hobby-scale product; going
PDF-only " Factur-X later" ignores that Peppol BIS is the single highest-value
target today.

## Implementation sequence (for ticket authors)

1. #138 + #139 + #143 land the identifiers + tax-aware line model (prereqs).
2. New ticket: `einvoice.rs` UBL/Factur-X writers + archive + download routes.
3. New ticket: provider port + vault config + send + retry/audit.
4. #146's Settings gets the "E-invoicing" panel (participant ids, provider,
   per-invoice delivery badge).
