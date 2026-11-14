/* Stand-in for a Shipyard release binary in the launcher tests. */
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static const char *report;

static void on_signal(int number) {
    FILE *out = fopen(report, "w");
    if (out) { fprintf(out, "SIG:%d\n", number); fclose(out); }
    _exit(40 + number);
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "";
    if (!strcmp(mode, "--version")) {
        const char *version = getenv("FIXTURE_VERSION");
        printf("shipyard %s\n", version ? version : "1.0.0");
        return getenv("FIXTURE_VERSION_FAILS") ? 1 : 0;
    }
    if (!strcmp(mode, "args")) {
        for (int i = 2; i < argc; i++) printf("ARG:%s\n", argv[i]);
        return 0;
    }
    if (!strcmp(mode, "env")) {
        for (int i = 2; i < argc; i++) {
            const char *value = getenv(argv[i]);
            printf("ENV:%s=%s\n", argv[i], value ? value : "<unset>");
        }
        return 0;
    }
    if (!strcmp(mode, "exit") && argc > 2) return atoi(argv[2]);
    if (!strcmp(mode, "selfkill") && argc > 2) {
        signal(atoi(argv[2]), SIG_DFL);
        raise(atoi(argv[2]));
        return 99;
    }
    if (!strcmp(mode, "wait") && argc > 2) {
        report = argv[2];
        int numbers[] = {SIGTERM, SIGINT, SIGHUP, SIGUSR1};
        for (unsigned i = 0; i < sizeof numbers / sizeof *numbers; i++) signal(numbers[i], on_signal);
        char ready[4096];
        snprintf(ready, sizeof ready, "%s.ready", report);
        FILE *out = fopen(ready, "w");
        if (out) { fprintf(out, "%d\n", getpid()); fclose(out); }
        for (;;) pause();
    }
    fprintf(stderr, "unknown mode\n");
    return 2;
}
