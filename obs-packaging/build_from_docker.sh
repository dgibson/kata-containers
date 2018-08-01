#!/bin/bash
#
# Copyright (c) 2018 Intel Corporation
#
# SPDX-License-Identifier: Apache-2.0
#
[ -z "${DEBUG}" ] || set -o xtrace

set -o errexit
set -o nounset
set -o pipefail


script_dir=$( cd $( dirname "${BASH_SOURCE[0]}" ) && pwd )
cache_dir=${PWD}/obs-cache
#where packaing repo lives
packaging_repo_dir=$(cd "${script_dir}/.." && pwd )
#where results will be stored
host_datadir="${PWD}/pkgs"
obs_image="obs-kata"
export USE_DOCKER=1
http_proxy=${http_proxy:-}
https_proxy=${https_proxy:-}
no_proxy=${no_proxy:-}
PUSH=${PUSH:-}


GO_ARCH=$(go env GOARCH)
export GO_ARCH
sudo docker build \
	--build-arg http_proxy="${http_proxy}" \
	--build-arg https_proxy="${https_proxy}" \
	-t $obs_image "${script_dir}"

pushd "${script_dir}/kata-containers-image/" >> /dev/null
	echo "Building image"
	./build_image.sh
popd >> /dev/null

sudo docker run \
	--rm \
	-v "${HOME}/.ssh":/root/.ssh \
	-v "${HOME}/.gitconfig":/root/.gitconfig \
	-v /etc/profile:/etc/profile \
	--env http_proxy="${http_proxy}" \
	--env https_proxy="${https_proxy}" \
	--env no_proxy="${no_proxy}" \
	--env PUSH="${PUSH}" \
	-v "${HOME}/.bashrc":/root/.bashrc \
	-v "$cache_dir":/var/tmp/osbuild-packagecache/ \
	-v "$packaging_repo_dir":${packaging_repo_dir} \
	-v "$host_datadir":/var/packaging \
	-v "$HOME/.oscrc":/root/.oscrc \
	-ti "$obs_image" bash -c "${packaging_repo_dir}/obs-packaging/build_all.sh"
