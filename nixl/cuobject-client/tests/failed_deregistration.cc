// Test-only replacement for the SDK's deregistration operation.
#include <cuobjclient.h>

cuObjErr_t cuObjClient::cuMemObjPutDescriptor(void *) {
    return CU_OBJ_INVALID_VALUE;
}
