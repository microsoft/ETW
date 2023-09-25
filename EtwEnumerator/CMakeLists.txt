cmake_minimum_required(VERSION 3.16)
project(eventheader-decode-cpp
    VERSION 1.0.0
    DESCRIPTION "ETW decoding and formatting for C++"
    HOMEPAGE_URL "https://github.com/microsoft/ETW"
    LANGUAGES CXX)
include(GNUInstallDirs)
include(CMakePackageConfigHelpers)
set(BUILD_SAMPLES ON CACHE BOOL "Build sample code")

add_compile_options(/W4 /WX /permissive-)

add_subdirectory(src)

if(BUILD_SAMPLES)
    add_subdirectory(samples)
endif()
