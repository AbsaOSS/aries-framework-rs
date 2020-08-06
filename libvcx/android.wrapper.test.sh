#!/usr/bin/env bash

# TODO: Move this to wrapper folder

set -ex

WORKDIR="$( cd "$(dirname "$0")" ; pwd -P )"
CI_DIR="${WORKDIR}/../ci/scripts"
export ANDROID_BUILD_FOLDER="/tmp/android_build"
JAVA_WRAPPER_DIR="${WORKDIR}/../wrappers/java"

TARGET_ARCH=$1

source ${CI_DIR}/setup.android.env.sh

if [ -z "${TARGET_ARCH}" ]; then
    echo STDERR "${RED}Missing TARGET_ARCH argument${RESET}"
    echo STDERR "${BLUE}e.g. x86 or arm${RESET}"
    exit 1
fi

test_wrapper(){
    pushd ${JAVA_WRAPPER_DIR}
        pushd android
            npm install
        popd

        # ANDROID_JNI_LIB=android/src/main/jniLibs
        # for arch in arm arm64 armv7 x86 x86_64
        # do
        #     arch_folder=${arch}
        #     if [ "${arch}" = "armv7" ]; then
        #         arch_folder="armeabi-v7a"
        #     elif [ "${arch}" = "arm64" ]; then
        #         arch_folder="arm64-v8a"
        #     fi
        #     rm ${ANDROID_JNI_LIB}/${arch_folder}/libc++_shared.so
        # done

        echo "Running :assembleDebugAndroidTest to see if it passes..."

        # Run the tests first
        ./gradlew --full-stacktrace --debug --no-daemon :assembleDebugAndroidTest --project-dir=android -x test

        echo "Installing the android test apk that will test the aar library..."
        i=0
        while
            sleep 10
            : ${start=$i}
            i="$((i+1))"
            ADB_INSTALL=$(adb install android/build/outputs/apk/androidTest/debug/com.evernym-vcx_1.0.0-*_x86-armv7-debug-androidTest.apk 2>&1)
            echo "ADB_INSTALL -- ${ADB_INSTALL}"
            FAILED_INSTALL=$(echo ${ADB_INSTALL}|grep "adb: failed to install")
            [ "${FAILED_INSTALL}" != "" ] && [ "$i" -lt 70 ]            # test the limit of the loop.
        do :;  done

        if [ "${FAILED_INSTALL}" != "" ]; then
            exit 1
        fi

        adb shell service list
        echo "Starting the tests of the aar library..."
        ./gradlew --full-stacktrace --debug --no-daemon :connectedCheck --project-dir=android
        cat ./android/build/reports/androidTests/connected/me.connect.VcxWrapperTests.html

        # for arch in arm arm64 armv7 x86 x86_64
        # do
        #     arch_folder=${arch}
        #     if [ "${arch}" = "armv7" ]; then
        #         arch_folder="armeabi-v7a"
        #     elif [ "${arch}" = "arm64" ]; then
        #         arch_folder="arm64-v8a"
        #     fi
        #     cp -v ../../../runtime_android_build/libvcx_${arch}/libc++_shared.so ${ANDROID_JNI_LIB}/${arch_folder}/libc++_shared.so
        # done
    popd
}

generate_arch_flags ${TARGET_ARCH}
setup_dependencies_env_vars ${TARGET_ARCH}
set_env_vars

download_sdk

create_standalone_toolchain_and_rust_target
create_cargo_config

build_libvcx

recreate_avd
check_if_emulator_is_running
test_wrapper
kill_avd
