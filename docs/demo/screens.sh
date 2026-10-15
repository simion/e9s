#!/bin/bash
# The README screenshots (docs/screens/*.png) from the made-up demo account: `make demo` runs this.
# Each screen is drawn by --script at 120x34 and rendered at 2x with headless Chrome.
set -e
cd "$(dirname "$0")/../.."
REAL_HOME=$HOME
export E9S_DEMO=1 HOME=$PWD/docs/demo/home
unset AWS_PROFILE AWS_CONFIG_FILE AWS_SHARED_CREDENTIALS_FILE AWS_REGION AWS_DEFAULT_REGION
CHROME=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
B=./target/release/e9s; T=$(mktemp -d); S="@size=120x34 @wait"
shot() { "$B" "${@:2}" >/dev/null 2>&1; HOME=$REAL_HOME "$CHROME" --headless=new --use-mock-keychain --disable-gpu --hide-scrollbars --force-device-scale-factor=2 \
  --window-size=976,548 --screenshot="$PWD/docs/screens/$1.png" "file://$T/$1.html" >/dev/null 2>&1; }
mkdir -p docs/screens
shot services -p prod -r us-east-1 --cluster shop-prod --script "$S @shot=$T/services"
shot rollout -p prod -r us-east-1 --cluster shop-prod --script "$S R @wait @shot=$T/rollout"
shot logs -p prod -r us-east-1 --cluster shop-prod --script "$S l @wait @sleep @shot=$T/logs"
shot metrics -p prod -r us-east-1 --cluster shop-prod --script "$S m @wait @shot=$T/metrics"
shot failures -p prod -r us-east-1 --cluster shop-prod --script "$S /image <Enter> F @wait @shot=$T/failures"
shot tasks -p prod -r us-east-1 --cluster shop-prod -s api --script "$S @shot=$T/tasks"
rm -r "$T"
ls docs/screens
