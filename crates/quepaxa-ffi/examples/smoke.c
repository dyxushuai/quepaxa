/* Drives a 3-replica in-process cluster through the C ABI until every replica commits
 * a proposal. Storage and network are in-memory queues; every persist completes at once.
 * Build: cc -I include examples/smoke.c target/release/libquepaxa_ffi.a -lpthread -ldl -lm */
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "quepaxa.h"

#define N 3
#define CAP 4096

typedef struct { uint32_t to; uint8_t *data; size_t len; } msg;

static msg net[CAP];
static size_t net_len;
static uint64_t committed[N];
static int saw_payload[N];

static void check(int32_t rc, const char *what) {
  if (rc < 0) {
    char detail[256];
    size_t len;
    if (qpx_last_error(detail, sizeof detail, &len) != QPX_OK) detail[0] = '\0';
    fprintf(stderr, "%s failed: %s (%s)\n", what, qpx_status_str(rc), detail);
    exit(1);
  }
}

static void enqueue(uint32_t to, qpx_bytes b) {
  assert(net_len < CAP);
  net[net_len].to = to;
  net[net_len].data = malloc(b.len);
  memcpy(net[net_len].data, b.ptr, b.len);
  net[net_len].len = b.len;
  net_len++;
}

/* Executes every pending action of replica i. Persists are "durable" immediately, but
 * qpx_persisted is a mutating call, so it waits until the actions are fully drained. */
static void drain(qpx_replica **r, int i) {
  uint64_t persisted[64];
  size_t n_persisted = 0;
  for (;;) {
    qpx_action a;
    int32_t rc = qpx_poll(r[i], &a);
    check(rc, "qpx_poll");
    if (rc == QPX_EMPTY) break;
    switch (a.kind) {
    case QPX_ACTION_SEND:
      enqueue(a.peer, a.bytes);
      break;
    case QPX_ACTION_PERSIST:
      for (size_t k = 0; k < a.count; k++) {
        qpx_bytes rec;
        check(qpx_action_record(r[i], k, &rec), "qpx_action_record");
        assert(rec.len > 0); /* a real host appends rec to its log and fsyncs */
      }
      for (size_t k = 0; k < a.then_count; k++) {
        uint32_t to;
        qpx_bytes b;
        check(qpx_action_then(r[i], k, &to, &b), "qpx_action_then");
        enqueue(to, b);
      }
      assert(n_persisted < 64);
      persisted[n_persisted++] = a.id;
      break;
    case QPX_ACTION_COMMIT:
      for (size_t k = 0; k < a.count; k++) {
        qpx_entry e;
        check(qpx_action_entry(r[i], k, &e), "qpx_action_entry");
        if (e.payload.len == 5 && memcmp(e.payload.ptr, "hello", 5) == 0) saw_payload[i] = 1;
      }
      committed[i] = a.slot + 1;
      break;
    default:
      break;
    }
  }
  if (n_persisted > 0) {
    /* Acknowledging the highest id covers every earlier persist. */
    check(qpx_persisted(r[i], 0, persisted[n_persisted - 1]), "qpx_persisted");
    drain(r, i);
  }
}

int main(void) {
  assert(qpx_abi_version() == ((QPX_ABI_VERSION_MAJOR << 16) | QPX_ABI_VERSION_MINOR));
  uint32_t members[N] = {0, 1, 2};
  qpx_replica *r[N];
  for (int i = 0; i < N; i++) {
    qpx_config cfg;
    check(qpx_config_init(&cfg, sizeof cfg), "qpx_config_init");
    memcpy(cfg.cluster_id, "c-smoke-cluster!", 16);
    cfg.id = (uint32_t)i;
    cfg.members = members;
    cfg.members_len = N;
    for (int k = 0; k < 32; k++) cfg.seed[k] = (uint8_t)(i * 31 + k); /* demo only: use a CSPRNG */
    check(qpx_replica_new(&cfg, NULL, 0, 0, &r[i]), "qpx_replica_new");
    drain(r, i);
  }
  uint64_t id;
  check(qpx_propose(r[1], 0, (const uint8_t *)"hello", 5, &id), "qpx_propose");
  drain(r, 1);
  for (int round = 0; round < 10000 && net_len > 0; round++) {
    msg m = net[0];
    memmove(net, net + 1, (net_len - 1) * sizeof(msg));
    net_len--;
    check(qpx_handle_message(r[m.to], 0, m.data, m.len), "qpx_handle_message");
    free(m.data);
    drain(r, (int)m.to);
  }
  for (int i = 0; i < N; i++) {
    if (!saw_payload[i] || committed[i] == 0) {
      fprintf(stderr, "replica %d did not commit (committed=%llu)\n", i, (unsigned long long)committed[i]);
      return 1;
    }
    qpx_replica_free(r[i]);
  }
  printf("c smoke ok: all %d replicas committed\n", N);
  return 0;
}
