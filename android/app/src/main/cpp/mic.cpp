// Microphone capture and Opus encoding for NativeMic.kt.
//
// AAudio in low-latency mode reads the microphone with a few milliseconds of buffering (on many
// phones straight from the audio hardware's buffer); AudioRecord keeps 10-20 ms more. libopus
// instead of Android's MediaCodec encoder allows short frames and the low-delay mode.

#include <aaudio/AAudio.h>
#include <android/log.h>
#include <jni.h>
#include <opus.h>

#include <algorithm>
#include <cmath>
#include <vector>

#define LOG(...) __android_log_print(ANDROID_LOG_INFO, "galdr", __VA_ARGS__)

namespace {

constexpr int kSampleRate = 48000;
constexpr int kMaxPacket = 1500;
constexpr int64_t kReadTimeoutNs = 100'000'000;  // lets the caller check for stop

struct Mic {
    AAudioStream* stream = nullptr;
    OpusEncoder* encoder = nullptr;
    int frame = 0;
    std::vector<float> pcm;
    unsigned char packet[kMaxPacket];
    float peak = 0;

    ~Mic() {
        if (stream) {
            AAudioStream_requestStop(stream);
            AAudioStream_close(stream);
        }
        if (encoder) opus_encoder_destroy(encoder);
    }
};

Mic* from(jlong handle) { return reinterpret_cast<Mic*>(handle); }

void throwIo(JNIEnv* env, const char* what, int code, const char* (*text)(int)) {
    char message[160];
    snprintf(message, sizeof message, "%s: %s", what, text(code));
    env->ThrowNew(env->FindClass("java/io/IOException"), message);
}

const char* aaudioText(int code) { return AAudio_convertResultToText(code); }

}  // namespace

extern "C" {

/** Opens the microphone (`deviceId` 0 = Android chooses) and an encoder for `frameMs` frames. */
JNIEXPORT jlong JNICALL Java_io_github_huntez_galdr_NativeMic_open(
    JNIEnv* env, jclass, jint deviceId, jint inputPreset, jboolean session, jint frameMs,
    jint bitrate) {
    auto* mic = new Mic();
    mic->frame = kSampleRate * frameMs / 1000;
    mic->pcm.resize(mic->frame);

    AAudioStreamBuilder* builder = nullptr;
    aaudio_result_t r = AAudio_createStreamBuilder(&builder);
    if (r == AAUDIO_OK) {
        AAudioStreamBuilder_setDirection(builder, AAUDIO_DIRECTION_INPUT);
        AAudioStreamBuilder_setPerformanceMode(builder, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY);
        // Falls back to shared by itself when the hardware stream is taken.
        AAudioStreamBuilder_setSharingMode(builder, AAUDIO_SHARING_MODE_EXCLUSIVE);
        AAudioStreamBuilder_setSampleRate(builder, kSampleRate);
        AAudioStreamBuilder_setChannelCount(builder, 1);
        AAudioStreamBuilder_setFormat(builder, AAUDIO_FORMAT_PCM_FLOAT);
        AAudioStreamBuilder_setInputPreset(builder, inputPreset);
        if (deviceId != 0) AAudioStreamBuilder_setDeviceId(builder, deviceId);
        // Android's noise suppressor and gain control attach to a session.
        if (session) AAudioStreamBuilder_setSessionId(builder, AAUDIO_SESSION_ID_ALLOCATE);
        r = AAudioStreamBuilder_openStream(builder, &mic->stream);
        AAudioStreamBuilder_delete(builder);
    }
    if (r == AAUDIO_OK) r = AAudioStream_requestStart(mic->stream);
    if (r != AAUDIO_OK) {
        delete mic;
        throwIo(env, "microphone", r, aaudioText);
        return 0;
    }

    int err = OPUS_OK;
    // Restricted low delay: CELT only (fine at these bitrates), 2.5 ms look-ahead instead of 6.5.
    mic->encoder = opus_encoder_create(kSampleRate, 1, OPUS_APPLICATION_RESTRICTED_LOWDELAY, &err);
    if (err == OPUS_OK) err = opus_encoder_ctl(mic->encoder, OPUS_SET_BITRATE(bitrate));
    if (err == OPUS_OK) err = opus_encoder_ctl(mic->encoder, OPUS_SET_COMPLEXITY(10));
    if (err != OPUS_OK) {
        delete mic;
        throwIo(env, "opus", err, opus_strerror);
        return 0;
    }

    AAudioStream* s = mic->stream;
    LOG("microphone: device %d, preset %d, %s %s, burst %d frames, buffer %d, %d ms frames",
        AAudioStream_getDeviceId(s), inputPreset,
        AAudioStream_getSharingMode(s) == AAUDIO_SHARING_MODE_EXCLUSIVE ? "exclusive" : "shared",
        AAudioStream_getPerformanceMode(s) == AAUDIO_PERFORMANCE_MODE_LOW_LATENCY ? "low latency"
                                                                                  : "normal",
        AAudioStream_getFramesPerBurst(s), AAudioStream_getBufferSizeInFrames(s), frameMs);
    return reinterpret_cast<jlong>(mic);
}

JNIEXPORT jint JNICALL Java_io_github_huntez_galdr_NativeMic_sessionId(JNIEnv*, jclass, jlong h) {
    return AAudioStream_getSessionId(from(h)->stream);
}

JNIEXPORT jint JNICALL Java_io_github_huntez_galdr_NativeMic_frameSamples(JNIEnv*, jclass, jlong h) {
    return from(h)->frame;
}

/**
 * Waits for the next frame, applies `gain` and encodes it into `out` at `offset`. Returns the
 * packet size, 0 if no audio came within 100 ms; throws when the microphone is gone.
 */
JNIEXPORT jint JNICALL Java_io_github_huntez_galdr_NativeMic_encode(
    JNIEnv* env, jclass, jlong h, jbyteArray out, jint offset, jfloat gain) {
    Mic* mic = from(h);
    int got = 0;
    while (got < mic->frame) {
        aaudio_result_t r =
            AAudioStream_read(mic->stream, mic->pcm.data() + got, mic->frame - got, kReadTimeoutNs);
        if (r < 0) {
            throwIo(env, "microphone", r, aaudioText);
            return 0;
        }
        if (r == 0) return 0;  // timed out; partial frames are dropped with the next one
        got += r;
    }
    float peak = 0;
    for (float& v : mic->pcm) {
        v = std::clamp(v * gain, -1.0f, 1.0f);
        peak = std::max(peak, std::fabs(v));
    }
    mic->peak = peak;
    int n = opus_encode_float(mic->encoder, mic->pcm.data(), mic->frame, mic->packet, kMaxPacket);
    if (n < 0) {
        throwIo(env, "opus", n, opus_strerror);
        return 0;
    }
    if (offset + n > env->GetArrayLength(out)) return 0;
    env->SetByteArrayRegion(out, offset, n, reinterpret_cast<const jbyte*>(mic->packet));
    return n;
}

/** Peak of the last encoded frame, 0..1. */
JNIEXPORT jfloat JNICALL Java_io_github_huntez_galdr_NativeMic_peak(JNIEnv*, jclass, jlong h) {
    return from(h)->peak;
}

JNIEXPORT void JNICALL Java_io_github_huntez_galdr_NativeMic_close(JNIEnv*, jclass, jlong h) {
    delete from(h);
}

}  // extern "C"
