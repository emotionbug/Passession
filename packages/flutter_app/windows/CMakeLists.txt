# Project-level configuration.
cmake_minimum_required(VERSION 3.14)
project(paseo_sessions LANGUAGES CXX)

# The name of the executable created for the application. Change this to change
# the on-disk name of your application.
set(BINARY_NAME "paseo_sessions")

# Explicitly opt in to modern CMake behaviors to avoid warnings with recent
# versions of CMake.
cmake_policy(VERSION 3.14...3.25)

# Define build configuration option.
get_property(IS_MULTICONFIG GLOBAL PROPERTY GENERATOR_IS_MULTI_CONFIG)
if(IS_MULTICONFIG)
  set(CMAKE_CONFIGURATION_TYPES "Debug;Profile;Release"
    CACHE STRING "" FORCE)
else()
  if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE "Debug" CACHE
      STRING "Flutter build mode" FORCE)
    set_property(CACHE CMAKE_BUILD_TYPE PROPERTY STRINGS
      "Debug" "Profile" "Release")
  endif()
endif()
# Define settings for the Profile build mode.
set(CMAKE_EXE_LINKER_FLAGS_PROFILE "${CMAKE_EXE_LINKER_FLAGS_RELEASE}")
set(CMAKE_SHARED_LINKER_FLAGS_PROFILE "${CMAKE_SHARED_LINKER_FLAGS_RELEASE}")
set(CMAKE_C_FLAGS_PROFILE "${CMAKE_C_FLAGS_RELEASE}")
set(CMAKE_CXX_FLAGS_PROFILE "${CMAKE_CXX_FLAGS_RELEASE}")

# Use Unicode for all projects.
add_definitions(-DUNICODE -D_UNICODE)

# Compilation settings that should be applied to most targets.
#
# Be cautious about adding new options here, as plugins use this function by
# default. In most cases, you should add new options to specific targets instead
# of modifying this function.
function(APPLY_STANDARD_SETTINGS TARGET)
  target_compile_features(${TARGET} PUBLIC cxx_std_17)
  target_compile_options(${TARGET} PRIVATE /W4 /WX /wd"4100")
  target_compile_options(${TARGET} PRIVATE /EHsc)
  target_compile_definitions(${TARGET} PRIVATE "_HAS_EXCEPTIONS=0")
  target_compile_definitions(${TARGET} PRIVATE "$<$<CONFIG:Debug>:_DEBUG>")
endfunction()

# Flutter library and tool build rules.
set(FLUTTER_MANAGED_DIR "${CMAKE_CURRENT_SOURCE_DIR}/flutter")
add_subdirectory(${FLUTTER_MANAGED_DIR})

# firebase_core serves only Android/iOS push (MobileServices.supported), but
# Flutter registers its Windows plugin for every Windows target. The Firebase
# C++ SDK has no Windows ARM64 libraries, so an ARM64 runner cannot link it.
# For windows-arm64 only, build from configure-time copies of Flutter's
# generated plugin files without firebase_core; Flutter keeps regenerating the
# originals, which x64 builds use unchanged.
set(PASEO_PLUGIN_REGISTRANT_SOURCE
  "${FLUTTER_MANAGED_DIR}/generated_plugin_registrant.cc")
set(PASEO_GENERATED_PLUGINS_CMAKE "${FLUTTER_MANAGED_DIR}/generated_plugins.cmake")
if(FLUTTER_TARGET_PLATFORM STREQUAL "windows-arm64")
  set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS
    "${PASEO_PLUGIN_REGISTRANT_SOURCE}" "${PASEO_GENERATED_PLUGINS_CMAKE}")
  # Each expected generated entry must be present and leave no other
  # firebase_core reference, so a generator change fails instead of linking.
  function(paseo_remove_generated VAR PATTERN REPLACEMENT)
    string(REGEX REPLACE "${PATTERN}" "${REPLACEMENT}" FILTERED "${${VAR}}")
    if("${FILTERED}" STREQUAL "${${VAR}}")
      message(FATAL_ERROR "Flutter's generated plugin files changed; cannot "
        "exclude firebase_core from the windows-arm64 runner (${PATTERN}).")
    endif()
    set(${VAR} "${FILTERED}" PARENT_SCOPE)
  endfunction()
  file(READ "${PASEO_PLUGIN_REGISTRANT_SOURCE}" PASEO_REGISTRANT)
  paseo_remove_generated(PASEO_REGISTRANT
    "#include <firebase_core/firebase_core_plugin_c_api\\.h>\r?\n" "")
  paseo_remove_generated(PASEO_REGISTRANT
    "  FirebaseCorePluginCApiRegisterWithRegistrar\\([^;]*;\r?\n" "")
  file(READ "${PASEO_GENERATED_PLUGINS_CMAKE}" PASEO_PLUGINS)
  paseo_remove_generated(PASEO_PLUGINS "\n  firebase_core\r?\n" "\n")
  foreach(PASEO_FILTERED IN ITEMS PASEO_REGISTRANT PASEO_PLUGINS)
    string(REGEX MATCH "firebase_core|FirebaseCore" PASEO_LEFTOVER
      "${${PASEO_FILTERED}}")
    if(PASEO_LEFTOVER)
      message(FATAL_ERROR "Flutter's generated plugin files changed; "
        "firebase_core remains in the windows-arm64 runner.")
    endif()
  endforeach()

  set(PASEO_ARM64_PLUGIN_DIR "${CMAKE_CURRENT_BINARY_DIR}/paseo_arm64_plugins")
  configure_file("${FLUTTER_MANAGED_DIR}/generated_plugin_registrant.h"
    "${PASEO_ARM64_PLUGIN_DIR}/generated_plugin_registrant.h" COPYONLY)
  set(PASEO_PLUGIN_REGISTRANT_SOURCE
    "${PASEO_ARM64_PLUGIN_DIR}/generated_plugin_registrant.cc")
  set(PASEO_GENERATED_PLUGINS_CMAKE "${PASEO_ARM64_PLUGIN_DIR}/generated_plugins.cmake")
  # Rewrite only on change so unchanged configures do not force a rebuild.
  function(paseo_write_if_changed PATH CONTENT)
    if(EXISTS "${PATH}")
      file(READ "${PATH}" EXISTING)
      if("${EXISTING}" STREQUAL "${CONTENT}")
        return()
      endif()
    endif()
    file(WRITE "${PATH}" "${CONTENT}")
  endfunction()
  paseo_write_if_changed("${PASEO_PLUGIN_REGISTRANT_SOURCE}" "${PASEO_REGISTRANT}")
  paseo_write_if_changed("${PASEO_GENERATED_PLUGINS_CMAKE}" "${PASEO_PLUGINS}")
endif()

# Application build; see runner/CMakeLists.txt.
add_subdirectory("runner")


# JNI is used only by Android path_provider/shared_preferences implementations.
# Do not link a developer-machine JVM into native desktop packages.
set(CMAKE_DISABLE_FIND_PACKAGE_JNI TRUE)

# Generated plugin build rules, which manage building the plugins and adding
# them to the application.
include("${PASEO_GENERATED_PLUGINS_CMAKE}")

# permission_handler_windows 0.2.2 requests C++20 but also adds legacy /await,
# which makes C++/WinRT select <experimental/coroutine>. Use standard C++20
# coroutines instead; MSVC 14.51 rejects that deprecated implementation.
if(TARGET permission_handler_windows_plugin)
  get_target_property(PERMISSION_HANDLER_COMPILE_OPTIONS
    permission_handler_windows_plugin COMPILE_OPTIONS)
  list(REMOVE_ITEM PERMISSION_HANDLER_COMPILE_OPTIONS "/await")
  set_target_properties(permission_handler_windows_plugin PROPERTIES
    COMPILE_OPTIONS "${PERMISSION_HANDLER_COMPILE_OPTIONS}"
    CXX_STANDARD 20
    CXX_STANDARD_REQUIRED ON
    CXX_EXTENSIONS OFF
  )
endif()


# === Installation ===
# Support files are copied into place next to the executable, so that it can
# run in place. This is done instead of making a separate bundle (as on Linux)
# so that building and running from within Visual Studio will work.
set(BUILD_BUNDLE_DIR "$<TARGET_FILE_DIR:${BINARY_NAME}>")
# Make the "install" step default, as it's required to run.
set(CMAKE_VS_INCLUDE_INSTALL_TO_DEFAULT_BUILD 1)
if(CMAKE_INSTALL_PREFIX_INITIALIZED_TO_DEFAULT)
  set(CMAKE_INSTALL_PREFIX "${BUILD_BUNDLE_DIR}" CACHE PATH "..." FORCE)
endif()

set(INSTALL_BUNDLE_DATA_DIR "${CMAKE_INSTALL_PREFIX}/data")
set(INSTALL_BUNDLE_LIB_DIR "${CMAKE_INSTALL_PREFIX}")

install(TARGETS ${BINARY_NAME} RUNTIME DESTINATION "${CMAKE_INSTALL_PREFIX}"
  COMPONENT Runtime)

install(FILES "${FLUTTER_ICU_DATA_FILE}" DESTINATION "${INSTALL_BUNDLE_DATA_DIR}"
  COMPONENT Runtime)

install(FILES "${FLUTTER_LIBRARY}" DESTINATION "${INSTALL_BUNDLE_LIB_DIR}"
  COMPONENT Runtime)

if(PLUGIN_BUNDLED_LIBRARIES)
  install(FILES "${PLUGIN_BUNDLED_LIBRARIES}"
    DESTINATION "${INSTALL_BUNDLE_LIB_DIR}"
    COMPONENT Runtime)
endif()

# Copy the native assets provided by the build.dart from all packages.
set(NATIVE_ASSETS_DIR "${PROJECT_BUILD_DIR}native_assets/windows/")
install(DIRECTORY "${NATIVE_ASSETS_DIR}"
   DESTINATION "${INSTALL_BUNDLE_LIB_DIR}"
   COMPONENT Runtime)

# Fully re-copy the assets directory on each build to avoid having stale files
# from a previous install.
set(FLUTTER_ASSET_DIR_NAME "flutter_assets")
install(CODE "
  file(REMOVE_RECURSE \"${INSTALL_BUNDLE_DATA_DIR}/${FLUTTER_ASSET_DIR_NAME}\")
  " COMPONENT Runtime)
install(DIRECTORY "${PROJECT_BUILD_DIR}/${FLUTTER_ASSET_DIR_NAME}"
  DESTINATION "${INSTALL_BUNDLE_DATA_DIR}" COMPONENT Runtime)

# Install the AOT library on non-Debug builds only.
install(FILES "${AOT_LIBRARY}" DESTINATION "${INSTALL_BUNDLE_DATA_DIR}"
  CONFIGURATIONS Profile;Release
  COMPONENT Runtime)
