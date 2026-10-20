/* C smoke test of the emusync ABI, linked against the static library (tests/c_abi.rs builds and
   runs it): create -> add container -> session begin/end -> poll events -> destroy, offline. */
#define _CRT_SECURE_NO_WARNINGS
#include "emusync.h"

#include <stdio.h>
#include <string.h>

#define CHECK(cond)                                                                   \
    do {                                                                              \
        if (!(cond)) {                                                                \
            fprintf(stderr, "%s:%d: check failed: %s\n", __FILE__, __LINE__, #cond); \
            return 1;                                                                 \
        }                                                                             \
    } while (0)

static int log_lines;

static void on_log(void* user, es_log_level level, const char* target, const char* msg) {
    (void)user;
    (void)level;
    (void)target;
    (void)msg;
    log_lines++; /* also runs on the worker thread: an approximate count is enough here */
}

static void on_wake(void* user) {
    (void)user;
}

static int versions;

static void on_version(void* user, const es_version_info* v) {
    (void)user;
    if (strncmp(v->version_id, "b3:", 3) == 0) versions++;
}

static int write_file(const char* path, const char* text) {
    FILE* f = fopen(path, "wb");
    if (!f) return 0;
    fputs(text, f);
    return fclose(f) == 0;
}

int main(int argc, char** argv) {
    char data_dir[1024], card[1024];
    es_config cfg = ES_CONFIG_INIT;
    es_client* c = NULL;
    es_container_desc d = ES_CONTAINER_DESC_INIT;
    es_container_id id = 0;
    es_card_mode mode = ES_CARD_UNITS;
    es_title t = ES_TITLE_INIT;
    es_session_begin_args a = ES_SESSION_BEGIN_INIT;
    es_session_info info = ES_SESSION_INFO_INIT;
    es_event ev = ES_EVENT_INIT;
    es_status st;
    int started = 0, finished = 0, polled;

    CHECK(argc == 2);
    CHECK(es_abi_version() == ES_ABI_VERSION);
    printf("emusync %s\n", es_version_string());
    snprintf(data_dir, sizeof data_dir, "%s/data", argv[1]);
    snprintf(card, sizeof card, "%s/game.srm", argv[1]);
    CHECK(write_file(card, "battery save, v1"));

    cfg.data_dir = data_dir;
    cfg.device_name = "c-smoke";
    cfg.emulator_id = "smoke";
    cfg.log = on_log;
    cfg.log_level = ES_LOG_DEBUG;
    CHECK(es_client_create(&cfg, &c) == ES_OK && c != NULL);
    es_event_set_notifier(c, on_wake, NULL);

    d.location = card;
    CHECK(es_container_add(c, &d, &id) == ES_OK && id != 0);
    CHECK(es_card_get_mode(c, "sram:game", &mode) == ES_OK && mode == ES_CARD_WHOLE_FILE);

    t.platform = "snes";
    t.content_hash = "crc32:1234abcd";
    a.title = &t;
    a.mounted = &id;
    a.mounted_count = 1;
    a.timeout_ms = 2000;
    st = es_session_begin(c, &a, &info);
    CHECK(st == ES_OFFLINE); /* no remote */
    CHECK(info.id != 0 && info.remote_reached == 0 && info.other_device_active[0] == 0);
    CHECK(es_container_remove(c, id) == ES_E_BUSY);

    /* The game saves. */
    CHECK(write_file(card, "battery save, v2"));
    es_container_touched(c, id);
    CHECK(es_container_flushed(c, id) == ES_OK);
    CHECK(es_session_end(c, info.id, 60000) == ES_PENDING);
    CHECK(es_session_end(c, info.id, 0) == ES_E_NOT_FOUND);

    /* Waits for the worker, so everything queued before it has run. */
    CHECK(es_unit_history(c, "sram:game", on_version, NULL) == ES_OK);
    CHECK(versions == 2);
    CHECK(es_state_publish(c, NULL) == ES_E_INVALID_ARG);
    /* Encryption: no remote to unlock yet; a passphrase is required. */
    CHECK(es_repo_unlock(c, "secret") == ES_E_INVALID_ARG);
    CHECK(es_repo_create_encrypted(c, "") == ES_E_INVALID_ARG);
    CHECK(es_repo_create_encrypted(c, "secret") == ES_OK);
    CHECK(strcmp(es_status_str(ES_E_LOCKED), "repository locked (passphrase needed)") == 0);

    while ((polled = es_event_poll(c, &ev)) == 1) {
        if (ev.type == ES_EV_SYNC_STARTED) started++;
        if (ev.type == ES_EV_SYNC_FINISHED) {
            finished++;
            CHECK(ev.status == ES_OFFLINE);
        }
        es_event_release(c, &ev);
        CHECK(ev.message == NULL);
    }
    CHECK(polled == 0);
    CHECK(started >= 2 && finished >= 2); /* session begin and session end */
    CHECK(log_lines > 0);

    es_client_destroy(c, 5000);
    printf("ok: %d sync cycles, %d log lines\n", finished, log_lines);
    return 0;
}
