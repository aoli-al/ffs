#include <jni.h>
#include "ffs.h"

JNIEXPORT void JNICALL Java_Ffs_begin(JNIEnv *env, jclass cls) { ffs_begin(); }
JNIEXPORT void JNICALL Java_Ffs_end(JNIEnv *env, jclass cls) { ffs_end(); }
JNIEXPORT void JNICALL Java_Ffs_yield(JNIEnv *env, jclass cls) { ffs_yield(); }
JNIEXPORT void JNICALL Java_Ffs_check(JNIEnv *env, jclass cls, jboolean ok, jstring message) {
    const char *text = (*env)->GetStringUTFChars(env, message, NULL);
    if (text == NULL) return;
    ffs_assert(ok, text);
    (*env)->ReleaseStringUTFChars(env, message, text);
    if (!ok) {
        jclass error = (*env)->FindClass(env, "java/lang/AssertionError");
        if (error != NULL) (*env)->ThrowNew(env, error, "FFS property failed");
    }
}
