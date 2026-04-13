#!/bin/sh
echo "${GREETING} from Docksmith! Version: ${APP_VERSION}"
echo "Running as: $(id)"
echo "Working dir: $(pwd)"
echo "Files here: $(ls /app)"
