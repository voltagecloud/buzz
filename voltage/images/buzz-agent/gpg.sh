#!/bin/sh
# Fail if the developer's forwarded signer is absent; never create a local signer.
exec /usr/bin/gpg --no-autostart "$@"
