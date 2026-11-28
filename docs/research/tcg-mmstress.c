// Address-space churn check for docs/research/tcg-context-switch.md: does a guest see stale
// translations when its soft TLB survives a context switch?
//
//   gcc -O2 -static -o mmstress tcg-mmstress.c
//   mmstress <workers> <seconds>
//
// Each worker (one process, all on the same few vCPUs so they switch constantly) loops over
// mmap, fill with a pattern that names the worker and the pass, fork (copy-on-write: the child
// writes every page and checks its own copy, the parent checks that its copy did not change),
// mprotect to read-only and back, mremap, munmap and a fresh mmap at the same address. Any page
// that reads back the wrong pattern is a stale or crossed translation: it is counted and the
// first few are printed. Prints "workers <n> passes <n> pages_checked <n> bad <n>".
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define PAGES 96
#define PG 4096

static uint64_t pat(int w, uint64_t pass, int page) { return ((uint64_t)w << 48) ^ (pass << 16) ^ (uint64_t)page ^ 0x5a5a5a5a00000000ull; }

static uint64_t fill(uint64_t *p, int w, uint64_t pass) {
    for (int i = 0; i < PAGES; i++) p[i * PG / 8] = pat(w, pass, i);
    return 0;
}
static int check(uint64_t *p, int w, uint64_t pass, const char *what) {
    int bad = 0;
    for (int i = 0; i < PAGES; i++) {
        if (p[i * PG / 8] != pat(w, pass, i)) {
            if (bad++ < 2) fprintf(stderr, "worker %d pass %lu page %d %s: %016lx want %016lx\n", w, (unsigned long)pass, i, what, (unsigned long)p[i * PG / 8], (unsigned long)pat(w, pass, i));
        }
    }
    return bad;
}

int main(int argc, char **argv) {
    int workers = argc > 1 ? atoi(argv[1]) : 8;
    int secs = argc > 2 ? atoi(argv[2]) : 30;
    int fds[64][2];
    for (int w = 0; w < workers; w++) {
        if (pipe(fds[w])) return 1;
        if (fork() == 0) {
            uint64_t passes = 0, checked = 0, bad = 0;
            time_t end = time(NULL) + secs;
            void *hint = (void *)(0x100000000ull + (uint64_t)w * 0x10000000ull);
            while (time(NULL) < end) {
                uint64_t *p = mmap(hint, PAGES * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
                if (p == MAP_FAILED) { perror("mmap"); exit(2); }
                uint64_t pass = ++passes;
                fill(p, w, pass);
                pid_t c = fork();
                if (c == 0) {
                    // the child's copy: every page is written (copy-on-write), then checked
                    uint64_t *q = p;
                    int cb = check(q, w, pass, "child sees parent's page");
                    for (int i = 0; i < PAGES; i++) q[i * PG / 8] = pat(w + 100, pass, i);
                    cb += check(q, w + 100, pass, "child after write");
                    _exit(cb ? 1 : 0);
                }
                // the parent keeps writing while the child copies; its own pages must stay its own
                for (int i = 0; i < PAGES; i += 2) p[i * PG / 8] = pat(w, pass, i);
                int st;
                waitpid(c, &st, 0);
                if (WIFEXITED(st) && WEXITSTATUS(st)) bad += 1;
                bad += check(p, w, pass, "parent after fork");
                mprotect(p, PAGES * PG, PROT_READ);
                bad += check(p, w, pass, "read-only");
                mprotect(p, PAGES * PG, PROT_READ | PROT_WRITE);
                fill(p, w, pass + 1000000);
                bad += check(p, w, pass + 1000000, "rewritten");
                void *r = mremap(p, PAGES * PG, 2 * PAGES * PG, MREMAP_MAYMOVE);
                if (r == MAP_FAILED) { perror("mremap"); exit(2); }
                bad += check(r, w, pass + 1000000, "mremap");
                munmap(r, 2 * PAGES * PG);
                checked += 6 * PAGES;
            }
            uint64_t out[3] = {passes, checked, bad};
            if (write(fds[w][1], out, 24) != 24) exit(3);
            _exit(0);
        }
    }
    uint64_t tp = 0, tc = 0, tb = 0;
    for (int w = 0; w < workers; w++) {
        uint64_t v[3];
        if (read(fds[w][0], v, 24) != 24) {
            fprintf(stderr, "worker %d died\n", w);
            tb++;
            continue;
        }
        tp += v[0], tc += v[1], tb += v[2];
    }
    while (wait(NULL) > 0) {}
    printf("workers %d passes %lu pages_checked %lu bad %lu\n", workers, (unsigned long)tp, (unsigned long)tc, (unsigned long)tb);
    return tb != 0;
}
