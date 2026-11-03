#!/usr/bin/env bash
# Run from the devShell (direnv) so the JDK 21 `java` is on PATH.
cd "$(dirname "$0")"
exec java -Xmx4G -jar server.jar nogui
