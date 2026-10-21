#!/bin/bash
printf '#!/bin/sh\necho new\n' > run.sh
chmod 755 run.sh
ln -s run.sh latest
ln run.sh again.sh
rm old.txt
mkdir -p /root/.cache/pip && echo wheel > /root/.cache/pip/w
