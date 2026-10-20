# Security

openTessera stores Plex tokens and Jellyfin API keys in
`~/.config/opentessera/` (owner-readable only), and its API listens on a
local Unix socket. If you find a way to get at those credentials, or to make
the app do something through that socket or a crafted file or stream that
it shouldn't, please report it privately rather than in a public issue.

Use GitHub's **Report a vulnerability** button on the repository's
**Security** tab. Reports are read by the one maintainer, in spare time, so
expect a reply in days rather than hours.
