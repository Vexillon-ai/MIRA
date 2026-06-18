---
title: Single sign-on (OIDC & LDAP)
description: Let people log in to MIRA with Google, Microsoft Entra, Keycloak, Okta, or your LDAP / Active Directory — without giving up the local admin login.
sidebar:
  order: 9
---

MIRA can delegate login to an identity provider you already run, so people sign
in with their existing company or Google account instead of a MIRA-specific
password. Two mechanisms are supported, both **off by default**:

- **OIDC (OpenID Connect)** — a "Sign in with …" button for any standard
  provider: Google, Microsoft Entra ID, Keycloak, Authentik, Okta, and more.
- **LDAP / Active Directory** — username/password checked against your
  directory.

The most important guarantee first: **local login always works.** SSO and LDAP
are *added* alongside local username/password — never instead of it. A bootstrap
admin account or a directory outage can never lock everyone out, because a
local password login is always available.

> Provider changes take effect on a **service restart**. Plan a brief restart
> when you add or reconfigure a provider.

For adding the accounts these identities map onto, see
[Manage users & access](manage-users.md).

## OIDC (Google, Entra, Keycloak, Okta, …)

OIDC is **generic and discovery-driven** — one configuration shape covers any
compliant provider. You don't pick a provider type; you point MIRA at the
provider's issuer URL and it discovers the rest.

### 1. Register MIRA at your provider

In your identity provider, create an **OAuth/OIDC application** (a confidential
client). You'll get a **client id** and **client secret**. Set the **redirect
URI** to:

```
<your-mira-base-url>/api/auth/oidc/callback
```

It must match exactly. MIRA builds this from `auth.oidc.public_base_url` — the
origin (scheme + host + port) browsers reach MIRA on — so set that to your real
public URL (e.g. `https://mira.example.com`). If left empty it defaults to
`http://127.0.0.1:<server.port>`, which is fine for local testing only.

### 2. Configure the provider in MIRA

Under `auth.oidc` (Settings → Server, or `mira_config.json`):

```json
{
  "auth": {
    "oidc": {
      "enabled": true,
      "public_base_url": "https://mira.example.com",
      "providers": [
        {
          "id": "google",
          "display_name": "Google",
          "issuer": "https://accounts.google.com",
          "client_id": "…apps.googleusercontent.com",
          "client_secret": "…",
          "auto_provision": false,
          "allowed_domains": []
        }
      ]
    }
  }
}
```

The fields:

- **`enabled`** — master switch. Even with providers listed, OIDC is inert until
  this is `true`.
- **`id`** — a stable slug used in URLs and identity binding (e.g. `google`).
- **`display_name`** — the button label (e.g. `Google`, `Company SSO`).
- **`issuer`** — the discovery base URL. MIRA fetches
  `<issuer>/.well-known/openid-configuration` to resolve the authorization,
  token, and userinfo endpoints.
- **`client_id`** / **`client_secret`** — the credentials from step 1.
- **`scopes`** — requested scopes; defaults to `openid email profile`.
- **`auto_provision`** / **`allowed_domains`** / **`default_role`** — see
  [Linking and auto-provisioning](#linking-and-auto-provisioning).

After a restart, the web login page automatically shows a **"Sign in with …"**
button for each configured provider. List several providers to show several
buttons.

### How the login flow works

The flow is **server-mediated** (Authorization Code + PKCE): MIRA redirects the
user to the IdP, then exchanges the code and reads the IdP's **userinfo**
endpoint server-side. **No token ever rides the redirect URL.** After the
round-trip MIRA issues *its own* session — the same JWT access token and
refresh cookie a password login produces — so SSO users get the same session
lifecycle (including [Sign out everywhere](manage-users.md#sign-out-everywhere))
as everyone else.

### Linking and auto-provisioning

MIRA resolves the incoming identity to a MIRA account in this order:

1. **By `(issuer, subject)`** — the stable identity MIRA recorded last time this
   person signed in via this provider.
2. **By email** — if no identity match, MIRA **links** the SSO identity to an
   existing local account with the same email.
3. **Auto-provision** — if there's still no match and **`auto_provision`** is on
   *and* the email's domain is in **`allowed_domains`**, MIRA **creates** a new
   account on first login, with the role from **`default_role`** (`user` by
   default).

`auto_provision` is **off by default** (link-to-existing-email only). Before
turning it on, set `allowed_domains` (lowercase, no `@`) so only your own
domains can self-create accounts — otherwise anyone the IdP authenticates could
provision an account.

## LDAP / Active Directory

LDAP login is tried **as a fallback when local password auth fails**. This is
what keeps local accounts working: MIRA checks the local password first, and
only if that fails does it try the directory. A directory outage therefore never
blocks the bootstrap admin or any local account.

Configure it under `auth.ldap`:

```json
{
  "auth": {
    "ldap": {
      "enabled": true,
      "url": "ldaps://dc.example.com:636",
      "starttls": false,
      "bind_dn": "cn=svc-mira,ou=service,dc=example,dc=com",
      "bind_password": "…",
      "user_base_dn": "ou=people,dc=example,dc=com",
      "user_filter": "(sAMAccountName={username})",
      "required_group": "",
      "auto_provision": false
    }
  }
}
```

The key fields:

- **`enabled`** — master switch.
- **`url`** — directory URL, e.g. `ldap://dc.example.com:389` or
  `ldaps://dc.example.com:636`.
- **`starttls`** — upgrade a plaintext `ldap://` connection to TLS before
  binding (ignored for `ldaps://`).
- **`bind_dn`** / **`bind_password`** — a service account used to *search* for
  the user before binding as them. Empty `bind_dn` means anonymous search.
- **`user_base_dn`** — the base DN to search under, e.g.
  `ou=people,dc=example,dc=com`.
- **`user_filter`** — the search filter; `{username}` is substituted
  (LDAP-escaped). Default `(uid={username})`; Active Directory usually wants
  `(sAMAccountName={username})`.
- **`required_group`** — if set, the user must be a member of this group DN
  (checked via `memberOf`). Empty means no group requirement.
- **`attr_email`** / **`attr_display_name`** — attributes to read for email and
  display name (defaults `mail` and `cn`).
- **`auto_provision`** / **`allowed_domains`** / **`default_role`** — same as
  OIDC: create a MIRA account on first successful LDAP login when there's no
  existing match. Off by default; scope it with `allowed_domains`.

MIRA authenticates with **search-then-bind**: it binds as the service account
(or anonymously), searches `user_base_dn` with `user_filter` to find the user's
DN, then binds as that user with the supplied password. On success the directory
identity is mapped to a MIRA user the same way OIDC does — link by
username/email, or auto-provision.

## Troubleshooting

- **No "Sign in with …" button appears.** Confirm `auth.oidc.enabled` is `true`,
  at least one provider is configured, and you've **restarted** the service.
- **Redirect URI mismatch at the IdP.** The URI registered at the provider must
  exactly equal `<public_base_url>/api/auth/oidc/callback`. Check
  `public_base_url` matches your real public origin (scheme, host, and port).
- **SSO/LDAP user can't get in but should.** With `auto_provision` off, the
  person needs a pre-existing MIRA account with a **matching email** to link to.
  Either add the account first (see
  [Manage users](manage-users.md#add-a-user-directly)) or enable
  `auto_provision` with the right `allowed_domains`.
- **LDAP login fails for everyone.** Verify `user_filter` matches your directory
  (`uid=` for OpenLDAP, `sAMAccountName=` for AD) and that the `bind_dn` service
  account can search `user_base_dn`. Local logins still work regardless.

## Related

- [Manage users & access](manage-users.md) — accounts, invites, roles, and
  capability RBAC.
- [Security & multi-user](../concepts/security-and-multi-user.md) — where SSO
  fits in MIRA's identity and isolation model.
