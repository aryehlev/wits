/* One thread takes a mutex and keeps it; the main thread queues for it. */
#include <pthread.h>
#include <sys/prctl.h>
#include <unistd.h>

static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;

static void *holder(void *arg) {
    (void)arg;
    prctl(PR_SET_NAME, "holder");
    pthread_mutex_lock(&m);
    for (;;)
        sleep(1);
    return 0;
}

int main(void) {
    pthread_t t;
    pthread_create(&t, 0, holder, 0);
    sleep(1);
    pthread_mutex_lock(&m);
    return 0;
}
