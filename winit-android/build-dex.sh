#!/usr/bin/env bash
# Rebuilds dnd.dex from the Java sources under java/.
#
# Needs javac and an Android SDK. ANDROID_HOME points at the SDK, BUILD_TOOLS
# names the build tools directory holding d8, and PLATFORM names the platform
# whose android.jar is compiled against.
set -e -u -o pipefail

cd "$(dirname "$0")"

ANDROID_HOME=${ANDROID_HOME:-$HOME/Android/Sdk}
BUILD_TOOLS=${BUILD_TOOLS:-36.0.0}
PLATFORM=${PLATFORM:-android-35}

ANDROID_JAR=$ANDROID_HOME/platforms/$PLATFORM/android.jar
D8=$ANDROID_HOME/build-tools/$BUILD_TOOLS/d8
OUT=dex-out

find java -name '*.class' -delete
rm -rf "$OUT"
mkdir "$OUT"

javac --source 8 --target 8 --boot-class-path "$ANDROID_JAR" -Xlint:deprecation \
    $(find java -name '*.java')

"$D8" --min-api 26 --classpath "$ANDROID_JAR" --output "$OUT" \
    $(find java -name '*.class')
mv "$OUT/classes.dex" dnd.dex
rm -rf "$OUT"

find java -name '*.class' -delete
