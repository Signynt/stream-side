// android/app/src/main/java/com/example/streamreceiver/MainActivity.kt
package com.example.streamreceiver

import android.os.Bundle
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.activity.enableEdgeToEdge
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat

// Разрешение, которое мы ожидаем от sender-а.
// В реальном приложении согласовывается через сигнальный протокол.
private const val STREAM_WIDTH  = 1920
private const val STREAM_HEIGHT = 1080
private const val SENDER_HOST   = "192.168.1.5"  // IP ПК с запущенным sender-ом
private const val SENDER_PORT   = 4433

class MainActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()

        // Не гасить экран пока приложение активно
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        val windowInsetsController = WindowCompat.getInsetsController(window, window.decorView)
        windowInsetsController.systemBarsBehavior = 
            WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        windowInsetsController.hide(WindowInsetsCompat.Type.systemBars())

        window.attributes.layoutInDisplayCutoutMode = 
            WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
        setContent {
            MaterialTheme {
                StreamReceiverScreen()
            }
        }
    }
}

@Composable
fun StreamReceiverScreen() {
    var statusText by remember { mutableStateOf("Ожидание Surface...") }

    LaunchedEffect(statusText) {
        if (statusText.startsWith("Подключено")) {
            kotlinx.coroutines.delay(3000) // Ждем 3 секунды
            statusText = "" // Очищаем текст
        }
    }

    Box(
        modifier = Modifier.fillMaxSize(),
        contentAlignment = Alignment.Center
    ) {
        // ── SurfaceView через AndroidView ────────────────────────────────
        //
        // Мы используем SurfaceView (а не TextureView), потому что:
        // 1. SurfaceView имеет выделенный оконный слой (dedicated window layer),
        //    что позволяет аппаратному кодеку рисовать напрямую, минуя GPU-композитинг.
        // 2. TextureView всегда идёт через SurfaceTexture → OpenGL-текстура → GPU-compose,
        //    добавляя ~1-2 кадра задержки.
        // 3. SurfaceView — минимальная задержка отображения для MediaCodec.
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory  = { context ->
                SurfaceView(context).also { surfaceView ->
                    surfaceView.holder.addCallback(object : SurfaceHolder.Callback {

                        override fun surfaceCreated(holder: SurfaceHolder) {
                            // Surface готов — инициализируем Rust MediaCodec backend.
                            //
                            // holder.surface — это объект android.view.Surface,
                            // который Rust конвертирует в ANativeWindow через JNI.
                            // ANativeWindow_fromSurface увеличивает ref-count нативного
                            // окна, поэтому Surface-объект на Java стороне может быть
                            // безопасно GC'd после этого вызова.
                            NativeLib.initBackend(
                                surface = holder.surface,
                                width   = STREAM_WIDTH,
                                height  = STREAM_HEIGHT,
                            )

                            // Запускаем QUIC-клиент в фоновом потоке Rust
                            NativeLib.startNetworking(
                                host = SENDER_HOST,
                                port = SENDER_PORT,
                            )

                            statusText = "Подключено к $SENDER_HOST:$SENDER_PORT"

                        }

                        override fun surfaceChanged(
                            holder:  SurfaceHolder,
                            format:  Int,
                            width:   Int,
                            height:  Int,
                        ) {
                            // Surface изменил размер (поворот экрана и т.п.).
                            // MediaCodec с adaptive-playback=1 справится без перезапуска.
                            // Если adaptive playback не поддерживается — нужна реинициализация:
                            //   NativeLib.shutdownBackend()
                            //   NativeLib.initBackend(holder.surface, width, height)
                        }

                        override fun surfaceDestroyed(holder: SurfaceHolder) {
                            // КРИТИЧНО: вызвать ДО выхода из этого callback'а!
                            //
                            // Платформа уничтожит Surface сразу после возврата из
                            // surfaceDestroyed. Если к тому моменту Rust-код ещё
                            // держит ANativeWindow и пытается писать в него —
                            // это undefined behavior в NDK.
                            //
                            // shutdownBackend() вызывает AMediaCodec_stop() +
                            // AMediaCodec_delete() + ANativeWindow_release() в правильном порядке.
                            NativeLib.shutdownBackend()
                            statusText = "Surface уничтожен"
                        }
                    })
                }
            }
        )
        if (statusText.isNotEmpty()) {
            Card(
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .padding(bottom = 64.dp, start = 16.dp, end = 16.dp)
            ) {
                Text(
                    text = statusText,
                    modifier = Modifier.padding(8.dp),
                    style = MaterialTheme.typography.bodySmall,
                )
            }
        }
    }
}