cmake_minimum_required(VERSION 3.16)
project(SoapySdrHal CXX)

set(CMAKE_CXX_STANDARD 14)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE Release CACHE STRING "" FORCE)
endif()

find_package(SoapySDR CONFIG REQUIRED)
find_package(Threads REQUIRED)
find_program(CARGO cargo REQUIRED)

add_compile_options(-Wall -Wextra)

if(DEFINED ENV{CARGO_TARGET_DIR})
    set(SDRHAL_DEFAULT_TARGET_DIR "$ENV{CARGO_TARGET_DIR}")
else()
    set(SDRHAL_DEFAULT_TARGET_DIR "${CMAKE_CURRENT_BINARY_DIR}/cargo")
endif()
set(SDRHAL_CARGO_TARGET_DIR "${SDRHAL_DEFAULT_TARGET_DIR}" CACHE PATH
    "Cargo target directory for the Rust library")
set(SDRHAL_RUST_LIB
    "${SDRHAL_CARGO_TARGET_DIR}/release/${CMAKE_STATIC_LIBRARY_PREFIX}sdr_hal_soapy${CMAKE_STATIC_LIBRARY_SUFFIX}")

# Cargo tracks its own inputs, so it runs on every build and does nothing when current.
add_custom_target(sdrhal_rust ALL
    COMMAND ${CMAKE_COMMAND} -E env CARGO_TARGET_DIR=${SDRHAL_CARGO_TARGET_DIR}
            ${CARGO} build --release --locked -p sdr-hal-soapy
            --manifest-path ${CMAKE_CURRENT_SOURCE_DIR}/../Cargo.toml
    BYPRODUCTS ${SDRHAL_RUST_LIB}
    USES_TERMINAL
    COMMENT "Building the sdr-hal-soapy Rust library")

add_library(sdrhal_rust_lib STATIC IMPORTED)
set_target_properties(sdrhal_rust_lib PROPERTIES IMPORTED_LOCATION ${SDRHAL_RUST_LIB})
add_dependencies(sdrhal_rust_lib sdrhal_rust)

set(SDRHAL_SYSTEM_LIBS Threads::Threads ${CMAKE_DL_LIBS})
if(APPLE)
    list(APPEND SDRHAL_SYSTEM_LIBS "-framework CoreFoundation" "-framework Security")
else()
    list(APPEND SDRHAL_SYSTEM_LIBS m rt)
endif()

SOAPY_SDR_MODULE_UTIL(
    TARGET sdrhal
    SOURCES
        module/Device.cpp
        module/Registration.cpp
    LIBRARIES sdrhal_rust_lib ${SDRHAL_SYSTEM_LIBS})
target_include_directories(sdrhal PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/include")
add_dependencies(sdrhal sdrhal_rust)

if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    # The Rust runtime stays private to the module, so it cannot interpose on a
    # host application or another module that links its own.
    target_link_options(sdrhal PRIVATE "-Wl,--exclude-libs,ALL")
endif()

add_executable(sdrhal_smoke tools/sdrhal_smoke.cpp)
target_link_libraries(sdrhal_smoke PRIVATE SoapySDR)
install(TARGETS sdrhal_smoke RUNTIME DESTINATION bin)
