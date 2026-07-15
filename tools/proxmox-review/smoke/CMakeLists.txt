cmake_minimum_required(VERSION 3.20)
project(shipyard_review_smoke LANGUAGES CXX)

add_executable(shipyard-review-smoke main.cpp)
target_compile_features(shipyard-review-smoke PRIVATE cxx_std_20)
