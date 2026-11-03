# Security center

**Security** is where the operator watches certificates, exposed files, plain HTTP, access by IP address, and shared rate limits.

Agents use the `security` scope. **Operate** can change the rules. **Watch** can read the checks and the counts.

## SSL

The SSL tab counts domains by mode (Let's Encrypt, an uploaded certificate, or none) and lists each uploaded certificate with the days remaining. Events record certificate uploads you add later and certificates that are inside 14 days of expiry or already expired. A daily note is written at most once per certificate when a security check runs.

Uploading and replacing certificates still happens on **Certificates**. This page is the place to see whether they are healthy.

## Exposure checks

**Run check** requests a fixed list of paths on every domain this panel routes, except preview domains. The list covers paths that show up when a generated app is left open to the internet: environment files, WordPress admin and config, Git metadata, Firebase service-account names, cloud credentials, private keys, debug endpoints, and SQL dumps.

Rudder does not store the response body. A finding says the path answered and, when the body looks like a secret, that it looked like one. The summary does not include the secret.

A normal single-page app that returns its HTML shell for every path is not reported, except WordPress admin pages and any response that looks like a secret.

The same check requests `http://` for domains that are supposed to use HTTPS. A successful answer that never redirects to HTTPS is a finding. If the server has a public IP, Rudder also requests that IP with the domain's Host header and warns when the app answers, because a visitor can then bypass the name.

Checks run only against domains stored in this panel. Start one with **Run check** or `POST /api/v1/security/scans`. Findings from the latest finished check are `GET /api/v1/security/findings`.

## Plain HTTP and IP counters

The edge gate counts requests whose forwarded protocol is `http` and requests whose hostname is an IP address. **Security** shows the totals. They are warnings, not blocks. A domain with HTTPS redirect still answers the redirect on HTTP; the active check above is what tells you the redirect is missing.

## Rate limits

A rule has:

| Field | Meaning |
| --- | --- |
| Name | Shown on the page |
| Hostname | Blank means every app domain. The panel and the Access portal are skipped |
| Path prefix | `/` matches every path |
| Methods | Empty means every method |
| Requests and window | How many requests a key may make in that many seconds |
| Burst | Extra requests allowed above the steady rate |
| Key | Client IP, or the value of a request header |

Every matching rule must allow the request. When one blocks it, Rudder records that rule, the hostname, the path, and the client IP. Those rows are kept for 24 hours so you can see whether a real visitor is being stopped. The allowed and blocked totals for the last day are on the same page.

Traefik asks `GET /api/v1/edge/gate` before it forwards a gated domain. The gate address contains a secret that only the generated Traefik file has. A request without that secret is not found. Agents cannot call the gate.

Create and edit rules with `/api/v1/security/rate-limits`. Saving a rule republishes routing so the gate is attached to the domains it covers.

## Cloudflare Zero Trust

**Cloudflare** is an optional tab for putting the panel and the apps behind a Cloudflare Tunnel and Cloudflare Access, and reaching SSH through Cloudflare, so ports 80, 443, 3000, and 22 can be closed. It is off until you set it up. See [Cloudflare Zero Trust](cloudflare.md).

## API

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/api/v1/security/overview` | Counts for certificates, findings, HTTP, and IP |
| GET | `/api/v1/security/ssl` | Certificate days remaining and domain modes |
| GET | `/api/v1/security/ssl/events` | Recent SSL events |
| GET, POST | `/api/v1/security/scans` | List checks, or start one |
| GET | `/api/v1/security/findings` | Findings from the latest finished check |
| GET, POST | `/api/v1/security/rate-limits` | List or create a rule |
| PATCH, DELETE | `/api/v1/security/rate-limits/{id}` | Update or delete a rule |
| GET | `/api/v1/security/rate-limits/stats` | Allowed and blocked totals for one day |
| GET | `/api/v1/security/edge` | HTTP and IP totals, and recent blocks |
