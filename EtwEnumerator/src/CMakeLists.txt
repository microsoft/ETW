add_library(EtwEnumerator
    EtwEnumerator.cpp
    EtwEnumeratorCallbacks.cpp
    EtwEnumerator_DefaultConstruct.cpp
    EtwEnumerator_Format.cpp)
target_include_directories(EtwEnumerator
    PUBLIC
    "$<BUILD_INTERFACE:${PROJECT_SOURCE_DIR}/include/>"
    "$<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>")
target_precompile_headers(EtwEnumerator
    PRIVATE stdafx.h)
set(ETWENUMERATOR_HEADERS
    "${PROJECT_SOURCE_DIR}/include/eventheader/EventEnumerator.h"
    "${PROJECT_SOURCE_DIR}/include/eventheader/EventFormatter.h")
set_target_properties(EtwEnumerator PROPERTIES
    PUBLIC_HEADER "${ETWENUMERATOR_HEADERS}")
target_compile_features(EtwEnumerator
    PRIVATE cxx_std_17)
install(TARGETS EtwEnumerator
    EXPORT EtwEnumeratorTargets
    PUBLIC_HEADER DESTINATION ${CMAKE_INSTALL_INCLUDEDIR}/eventheader)
install(EXPORT EtwEnumeratorTargets
    FILE "EtwEnumeratorTargets.cmake"
    DESTINATION "${CMAKE_INSTALL_LIBDIR}/cmake/EtwEnumerator")
configure_package_config_file(
    "${CMAKE_CURRENT_SOURCE_DIR}/EtwEnumeratorConfig.cmake.in"
    "${CMAKE_CURRENT_BINARY_DIR}/EtwEnumeratorConfig.cmake"
    INSTALL_DESTINATION "${CMAKE_INSTALL_LIBDIR}/cmake/EtwEnumerator"
    NO_SET_AND_CHECK_MACRO
    NO_CHECK_REQUIRED_COMPONENTS_MACRO)
write_basic_package_version_file(
    "${CMAKE_CURRENT_BINARY_DIR}/EtwEnumeratorConfigVersion.cmake"
    COMPATIBILITY SameMinorVersion)
install(FILES
    "${CMAKE_CURRENT_BINARY_DIR}/EtwEnumeratorConfig.cmake"
    "${CMAKE_CURRENT_BINARY_DIR}/EtwEnumeratorConfigVersion.cmake"
    DESTINATION "${CMAKE_INSTALL_LIBDIR}/cmake/EtwEnumerator")
