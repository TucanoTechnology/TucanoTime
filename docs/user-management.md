# User Management

Administrators manage accounts under Settings > Users. Search by name or email,
filter by role/status, and use + User to create an account. Edit changes profile,
role, status and optional billing/cost rates. Rate inputs use decimal amounts;
the API and storage use integer minor units. Tasks have no rate overrides.

Passwords are set on creation or through the separate Change password dialog.
There is no invitation email or automated password-reset delivery. Password
fields are masked, never prefilled, cleared on close, and never returned by the
API. Passwords are hashed with Argon2id. Password, role and active-state changes
invalidate existing sessions; ordinary profile changes do not.

Self-deletion, self-deactivation and self-demotion are blocked. The storage
layer also enforces at least one active administrator and unique email addresses
under its writer lock. Concurrent stale updates return 409 and must be retried
after reloading. Email-index updates follow the authoritative user documents.

Delete is permitted only for accounts without entries, expenses, submissions,
reimbursement claims or a running timer. Deactivate accounts with history instead;
their records remain available to administrators. Audit events record account
changes but never passwords or hashes. Members cannot access user-management
endpoints or the Users Settings tab.

Endpoints: GET/POST /users, PUT/DELETE /users/{id}, PUT /users/{id}/password.
The checked-in OpenAPI contract documents their inputs, errors and public user
responses. Negative, fractional or out-of-range rates and unknown fields are
rejected before persistence.