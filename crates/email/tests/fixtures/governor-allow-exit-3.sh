#!/bin/sh
# Stand-in for a Governor that prints a valid allow verdict and then exits
# nonzero. The gate must refuse it on the exit status alone.
echo '{"decision": "allow"}'
exit 3
