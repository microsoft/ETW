add_executable(EtwEnumeratorDecode
    EtwEnumeratorDecode.cpp)
target_link_libraries(EtwEnumeratorDecode
    EtwEnumerator)
target_compile_features(EtwEnumeratorDecode
    PRIVATE cxx_std_17)
