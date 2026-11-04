#!/usr/bin/env bash
# Run from the devShell (direnv) so the JDK 21 `java` is on PATH.
#
# Type server commands here as usual. Programs (the benchmark, the day-length
# keeper in mc-agents) write commands to the named pipe console.fifo instead:
# set MC_CONSOLE=fifo:mc-server/console.fifo in .env.
cd "$(dirname "$0")"
[ -p console.fifo ] || mkfifo -m 600 console.fifo
# Open the pipe read-write so it never reports end-of-file between writers.
exec 3<>console.fifo
{
    cat <&3 &
    reader=$!
    cat
    kill "$reader" 2>/dev/null
} | java -Xmx4G -jar server.jar nogui
