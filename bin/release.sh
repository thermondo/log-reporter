#!/usr/bin/env bash

set -eo pipefail

echo "runing release script..."

# run the check command to be sure the binary is fine
/usr/local/bin/log_reporter check
