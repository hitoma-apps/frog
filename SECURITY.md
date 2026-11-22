# Security policy

## Reporting a vulnerability

Please don't open a public issue for a security problem. Report it privately through GitHub's **Security → Report a vulnerability** on this repository, which keeps the report between you and the maintainers until a fix is ready.

Include what you found, how to reproduce it, and which version or commit you tested. We'll reply as soon as we can, and credit you in the fix if you'd like.

## Scope

FROG reads mail over IMAP and keeps a local cache of headers and message bodies. Problems we most want to hear about:

- anything that lets a message run code, open a file, or fetch remote content without the user asking;
- ways to make a sender, subject, file name or link look different from what it is;
- leaks of the password, the cache or saved mail to other users or to disk in the wrong place;
- crashes or hangs caused by a crafted message or IMAP response.
