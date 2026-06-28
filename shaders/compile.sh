#!/usr/bin/env bash
# Recompile the GLSL shaders to the checked-in SPIR-V blobs (shaders/*.spv).
# Run this after editing a .vert/.frag and commit the updated .spv — the build
# uses these blobs directly when no shader compiler is available (e.g. on CI).
#
# Requires `glslc` (from shaderc / the Vulkan SDK).
set -euo pipefail
cd "$(dirname "$0")"

compile() { glslc -fshader-stage="$2" "$1" -o "$1.spv" && echo "  $1.spv"; }

echo "Compiling shaders -> SPIR-V:"
compile quad.vert  vertex
compile quad.frag  fragment
compile video.frag fragment
echo "Done. Commit the updated shaders/*.spv."
