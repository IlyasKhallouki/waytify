# waytify

Media control for Waybar: an MPRIS daemon, a bar module, and a GTK4 layer-shell
player window. See `docs/ARCHITECTURE.md` for the shape of it and
`docs/THEMING.md` for the selectors, which are public API.

## Spotify Web API

These rules apply to everything under `crates/waytify-core/src/spotify/`.

- **OpenAPI spec.** Take endpoint paths, parameters and response schemas from
  <https://developer.spotify.com/reference/web-api/open-api-schema.yaml>. Do not
  guess endpoints or field names.
- **Authorization.** Authorization Code with PKCE
  (<https://developer.spotify.com/documentation/web-api/tutorials/code-pkce-flow>)
  for anything user specific. Plain Authorization Code is acceptable only with a
  secure backend, which waytify does not have. Client Credentials only for
  public, non-user data. Never the Implicit Grant flow, which is deprecated.
- **Redirect URIs.** HTTPS, except `http://127.0.0.1` for local development.
  Never `http://localhost`, never wildcards. See
  <https://developer.spotify.com/documentation/web-api/concepts/redirect_uri>.
- **Scopes.** Only the minimum a feature needs
  (<https://developer.spotify.com/documentation/web-api/concepts/scopes>). Do not
  request broad scopes ahead of time.
- **Tokens.** Store them securely and never put a client secret in client-side
  code. Implement refresh
  (<https://developer.spotify.com/documentation/web-api/tutorials/refreshing-tokens>)
  and send the user back through authorization when a refresh token expires.
- **Rate limits.** On HTTP 429, back off exponentially and respect the
  `Retry-After` header. Never retry immediately or in a tight loop.
- **Deprecated endpoints.** Do not use them. Prefer `/playlists/{id}/items` over
  `/playlists/{id}/tracks`, and `/me/library` over the type-specific library
  endpoints.
- **Errors.** Handle every HTTP error code the schema documents. Read the
  returned message and turn it into something the user can act on.
- **Developer Terms** (<https://developer.spotify.com/terms>). Do not cache
  Spotify content beyond what is needed for immediate use. Attribute content to
  Spotify. Do not use the API to train machine learning models on Spotify data.

## Writing

No em dashes in prose, comments, or commit messages. Comments say why, not what.
