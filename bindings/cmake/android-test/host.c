#include "emusync.h"

const char* emusync_host_version(void) {
    return es_abi_version() == ES_ABI_VERSION ? es_version_string() : "";
}
