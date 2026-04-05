// android/app/src/main/java/com/example/streamreceiver/NativeLib.kt
package com.example.streamreceiver

import android.view.Surface

/**
 * JNI-мост к Rust-библиотеке `libstream_receiver.so`.
 *
 * Порядок вызовов:
 *   1. [initBackend]     — после того как Surface создан
 *   2. [startNetworking] — запустить приём видеопотока
 *   3. [shutdownBackend] — при уничтожении Surface (onPause / поворот экрана)
 */
object NativeLib {

    init {
        // Имя должно совпадать с [lib] name в Cargo.toml: `stream_receiver`
        System.loadLibrary("stream_receiver")
    }

    /**
     * Инициализировать аппаратный HEVC-декодер MediaCodec с переданным Surface.
     *
     * ВАЖНО: Вызывать только после того, как [Surface] полностью создан
     * (внутри `surfaceCreated` / `SurfaceHolder.Callback`).
     *
     * @param surface Android Surface, полученный из SurfaceView или SurfaceTexture.
     *                Rust немедленно извлечёт ANativeWindow и не будет хранить
     *                ссылку на Java-объект.
     * @param width   Ожидаемая ширина потока (должна совпадать с sender-ом).
     * @param height  Ожидаемая высота потока.
     */
    external fun initBackend(surface: Surface, width: Int, height: Int)

    /**
     * Запустить QUIC-клиент для подключения к sender-у на ПК.
     * Создаёт фоновый поток, не блокирует вызывающий поток.
     *
     * @param host IP-адрес ПК-sender'а (например, "192.168.1.5")
     * @param port Порт (по умолчанию 4433)
     */
    external fun startNetworking(host: String, port: Int)

    /**
     * Остановить декодер и освободить ANativeWindow.
     *
     * ОБЯЗАТЕЛЬНО вызывать из `surfaceDestroyed` ДО возврата из него,
     * пока платформа ещё не уничтожила Surface. Иначе — UB в NDK-коде.
     */
    external fun shutdownBackend()
}