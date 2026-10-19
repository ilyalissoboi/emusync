// The header must compile as C++ (11 and later) without warnings, including every INIT macro,
// and link against the static library.
#include "emusync.h"

#include <cstdio>

static_assert(sizeof(es_status) == 4, "es_status is an int");
static_assert(ES_ABI_VERSION == 1u, "ABI version");

int main() {
    es_config cfg = ES_CONFIG_INIT;
    es_container_desc d = ES_CONTAINER_DESC_INIT;
    es_title t = ES_TITLE_INIT;
    es_session_begin_args a = ES_SESSION_BEGIN_INIT;
    es_session_info info = ES_SESSION_INFO_INIT;
    es_state_desc sd = ES_STATE_DESC_INIT;
    es_state_info si = ES_STATE_INFO_INIT;
    es_conflict_info ci = ES_CONFLICT_INFO_INIT;
    es_event ev = ES_EVENT_INIT;
    if (cfg.struct_size != sizeof cfg || d.struct_size != sizeof d || t.struct_size != sizeof t ||
        a.struct_size != sizeof a || info.struct_size != sizeof info || sd.struct_size != sizeof sd ||
        si.struct_size != sizeof si || ci.struct_size != sizeof ci || ev.struct_size != sizeof ev) {
        return 1;
    }
    if (es_abi_version() != ES_ABI_VERSION) return 1;
    es_client* c = nullptr;
    if (es_client_create(nullptr, &c) != ES_E_INVALID_ARG || c != nullptr) return 1;
    if (es_event_poll(nullptr, &ev) != ES_E_INVALID_ARG) return 1;
    es_client_destroy(nullptr, 0);
    std::printf("%s\n", es_status_str(ES_E_NO_SPACE));
    return 0;
}
