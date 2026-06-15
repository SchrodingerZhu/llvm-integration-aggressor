// RISC-V acquire/release litmus test (message-passing).
//
// Purpose: decide whether the runner's silicon honors RVWMO acquire/release to
// the strength the C++11 model (and llvm-libc's rwlock) assumes. It mirrors the
// rwlock's unlock->lock handoff exactly:
//
//   releaser (unlock): write protected data, then a RELEASE rmw on the flag
//                      -> riscv: plain store ; amoadd.w.rl
//   acquirer (lock):   ACQUIRE cas on the flag, then read protected data
//                      -> riscv: lr.w.aq/sc.w ; load
//
// If .aq/.rl are honored, an acquirer that observes flag>=1 MUST see data==MAGIC.
// A single "flag set but data stale" observation is a hardware memory-model
// violation, independent of the lock, the kernel, or the futex.
//
// The cells live in MAP_SHARED|MAP_ANONYMOUS memory -- the same memory type as
// multiple_process_test, the case that actually hung. Two run modes:
//   thread : releaser/acquirer are threads in one process
//   proc   : releaser/acquirer are separate forked processes (cross-process
//            shared mapping + process-shared barrier)
// The litmus SPINS (no futex), so it isolates shared-memory ordering from the
// futex syscall path.
//
// Build (natively on the riscv runner): cc -O2 -pthread -o litmus litmus.c
// Run:   ./litmus {thread|proc} [passes]

#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sysinfo.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define N (1 << 16) // independent cells per pass (each used once -> no reuse race)
#define MAGIC 0x5a5a5a5a

// data and flag on separate cache lines to maximize the reorder window.
struct cell {
  _Alignas(64) int data;
  _Alignas(64) int flag;
};

// Everything shared lives in one MAP_SHARED region so the proc mode works.
struct shared {
  struct cell cells[N];
  pthread_barrier_t bar;
  long passes;
  long long observations;
  long long violations;
};

static struct shared *S;

static void pin(int cpu) {
  cpu_set_t set;
  CPU_ZERO(&set);
  CPU_SET(cpu, &set);
  if (sched_setaffinity(0, sizeof(set), &set) != 0)
    fprintf(stderr, "warning: could not pin to cpu %d\n", cpu);
}

static void releaser(int cpu) {
  pin(cpu);
  for (long p = 0; p < S->passes; p++) {
    for (int i = 0; i < N; i++) { // reset
      S->cells[i].data = 0;
      __atomic_store_n(&S->cells[i].flag, 0, __ATOMIC_RELAXED);
    }
    pthread_barrier_wait(&S->bar); // begin run phase
    for (int i = 0; i < N; i++) {
      S->cells[i].data = MAGIC;                                   // protected data
      __atomic_fetch_add(&S->cells[i].flag, 1, __ATOMIC_RELEASE); // amoadd.w.rl
    }
    pthread_barrier_wait(&S->bar); // end run phase
  }
}

static void acquirer(int cpu) {
  pin(cpu);
  for (long p = 0; p < S->passes; p++) {
    pthread_barrier_wait(&S->bar); // begin run phase
    for (int i = 0; i < N; i++) {
      for (;;) { // spin until we acquire-observe flag == 1
        int expected = 1;
        if (__atomic_compare_exchange_n(&S->cells[i].flag, &expected, 2, 0,
                                        __ATOMIC_ACQUIRE, __ATOMIC_RELAXED))
          break; // lr.w.aq/sc.w
      }
      int d = S->cells[i].data; // must be MAGIC if acquire is honored
      S->observations++;
      if (d != MAGIC)
        S->violations++;
    }
    pthread_barrier_wait(&S->bar); // end run phase
  }
}

static void *releaser_thr(void *a) { releaser((int)(intptr_t)a); return NULL; }
static void *acquirer_thr(void *a) { acquirer((int)(intptr_t)a); return NULL; }

int main(int argc, char **argv) {
  const char *mode = (argc > 1) ? argv[1] : "thread";
  long passes = (argc > 2) ? atol(argv[2]) : 20000;
  int ncpu = get_nprocs();
  int rel_cpu = 0, acq_cpu = (ncpu > 1) ? ncpu - 1 : 0;

  S = mmap(NULL, sizeof(*S), PROT_READ | PROT_WRITE,
           MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (S == MAP_FAILED) {
    perror("mmap");
    return 2;
  }
  memset(S, 0, sizeof(*S));
  S->passes = passes;

  pthread_barrierattr_t ba;
  pthread_barrierattr_init(&ba);
  pthread_barrierattr_setpshared(&ba, PTHREAD_PROCESS_SHARED);
  pthread_barrier_init(&S->bar, &ba, 2);

  printf("litmus mode=%s passes=%ld cells/pass=%d ncpu=%d (releaser cpu %d, "
         "acquirer cpu %d) mem=MAP_SHARED\n",
         mode, passes, N, ncpu, rel_cpu, acq_cpu);
  fflush(stdout);

  struct timespec t0, t1;
  clock_gettime(CLOCK_MONOTONIC, &t0);

  if (strcmp(mode, "proc") == 0) {
    pid_t pid = fork();
    if (pid < 0) {
      perror("fork");
      return 2;
    }
    if (pid == 0) {     // child = acquirer
      acquirer(acq_cpu);
      _exit(0);
    }
    releaser(rel_cpu);  // parent = releaser
    int st;
    waitpid(pid, &st, 0);
  } else {
    pthread_t rt, at;
    pthread_create(&rt, NULL, releaser_thr, (void *)(intptr_t)rel_cpu);
    pthread_create(&at, NULL, acquirer_thr, (void *)(intptr_t)acq_cpu);
    pthread_join(rt, NULL);
    pthread_join(at, NULL);
  }

  clock_gettime(CLOCK_MONOTONIC, &t1);
  double secs = (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9;

  printf("observations=%lld violations=%lld elapsed=%.1fs (%.0f Mobs/s)\n",
         S->observations, S->violations, secs, S->observations / 1e6 / secs);
  if (S->violations) {
    printf("RESULT: FAIL - hardware does NOT honor acquire/release on this "
           "memory (RVWMO violation)\n");
    return 1;
  }
  printf("RESULT: PASS - acquire/release honored\n");
  return 0;
}
