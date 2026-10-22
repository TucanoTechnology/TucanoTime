# Harvest import implementation status

Harvest import is being developed under #394. The first milestone (#395) is a
read-only parser; there is no upload endpoint, Import settings page or data
commit operation yet. Do not treat successful parsing as permission to insert
historical records through the ordinary entry/expense endpoints: their external
invoiced status and historical rate protections are not implemented yet.

## Implemented parser

`harvest::parse(ReportKind, bytes)` supports Harvest detailed time reports and
expense reports independently. It preserves source fields, including additional
columns, and returns normalized rows with logical record numbers and physical
line ranges. Quoted commas, escaped quotes, multiline notes, UTF-8 BOM and CRLF
are supported. Headers normalize case, surrounding whitespace and the optional
question mark on boolean column names.

Malformed CSV, invalid UTF-8, missing/duplicate headers and oversized input reject
the file. Limits are 1 MiB, 10,000 logical records, 64 columns and 80 bytes per
header. Row-level problems return structured issues without dropping or
truncating the source rows. A later preview must block confirmation while those
issues remain unresolved.

Hours use signed integer hundredths only in the parser's intermediate rows;
the persisted positive-only `Hours` model is unchanged. Amounts and rates use
integer minor units, never floats. Monetary grouping such as `"16,200.0"` is
accepted only with correctly placed separators. Unsupported precision, overflow,
invalid booleans and inconsistent exported time amounts are reported.

An equal-and-opposite time pair is identified only when the match is unique in
both directions and agrees on date, person/employee identity, customer, project,
task, currency, rates, amounts and billing status. Both rows remain in the
report. Duplicate candidates and unmatched negatives require explicit review;
the parser never applies corrections or writes records.

Expense approval and units remain source metadata. Foreign amounts or exchange
rates mentioned inside notes are never inferred. The project name does not
override a row's calendar year.

## Verification

Run the anonymized parser tests:

```sh
cargo test --lib harvest::tests
```

An opt-in ignored test can validate private exports locally. Set
`HARVEST_TIME_CSV` and `HARVEST_EXPENSE_CSV` to the two local file paths, then run:

```sh
cargo test --lib harvest::tests::private_exports_are_read_only_and_reconcile -- --ignored --nocapture
```

It reads the files without copying them or printing source row values; only
counts and field-level issues are reported. Never add real exports to Git or
upload them to issue trackers.

## Remaining delivery

1. Protected import provenance, historical rates/costs and external-invoice
   exclusion/locking across invoices and reports.
2. Explicit user/reference mapping and reconciliation previews.
3. Idempotent import history, duplicate review and journaled commit/recovery.
4. Admin API contracts and accessible Import settings workflow.
5. Contract, storage failure/recovery and browser verification before importing
   any real financial records.