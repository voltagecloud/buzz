#!/bin/sh
# Modified by Voltage: bind native app identity to the Flutter distribution.
# Sourced by Xcode before the Flutter build phase.
set -eu

brand=${BUZZ_APP_VARIANT:-}
seen=false
old_ifs=$IFS
IFS=','
for encoded in ${DART_DEFINES:-}; do
  decoded=$(printf '%s' "$encoded" | base64 --decode 2>/dev/null || printf '%s' "$encoded" | base64 -D 2>/dev/null || true)
  case "$decoded" in
    BUZZ_APP_VARIANT=*)
      if [ "$seen" = true ]; then
        echo 'error: BUZZ_APP_VARIANT must be supplied at most once.' >&2
        exit 1
      fi
      seen=true
      if [ "${decoded#BUZZ_APP_VARIANT=}" != "$brand" ]; then
        echo 'error: Native and Flutter app branding must match.' >&2
        exit 1
      fi
      ;;
  esac
done
IFS=$old_ifs

case "$brand" in
  '') ;;
  voltage-commons)
    if [ "${PRODUCT_BUNDLE_IDENTIFIER:-}" != cloud.voltage.workspace ] ||
       [ "${APP_DISPLAY_NAME:-}" != 'Voltage Commons' ] ||
       [ "${BUZZ_APP_ICON_NAME:-}" != CommonsAppIcon ] ||
       [ "${BUZZ_URL_SCHEME:-}" != voltage-workspace ] ||
       [ "${BUZZ_LAUNCH_STORYBOARD:-}" != CommonsLaunchScreen ]; then
      echo 'error: Commons requires its own bundle identifier, display name, icon, launch screen, and URL scheme.' >&2
      exit 1
    fi
    if [ "$seen" = false ]; then
      encoded=$(printf '%s' "BUZZ_APP_VARIANT=$brand" | base64 | tr -d '\n')
      DART_DEFINES=${DART_DEFINES:+$DART_DEFINES,}$encoded
      export DART_DEFINES
    fi
    ;;
  *) echo 'error: Unknown BUZZ_APP_VARIANT.' >&2; exit 1 ;;
esac
