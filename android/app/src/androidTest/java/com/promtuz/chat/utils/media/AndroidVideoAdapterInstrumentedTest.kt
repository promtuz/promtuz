package com.promtuz.chat.utils.media

import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.core.TimeRangeRecord
import uniffi.core.VideoEncodingPlanRecord
import uniffi.core.VideoRenderRequestRecord

@RunWith(AndroidJUnit4::class)
class AndroidVideoAdapterInstrumentedTest {
    @Test fun byteSourceIsReadThroughTheSameBoundedLease() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val bytes = ByteArray(1_024) { it.toByte() }
        val adapter = AndroidMediaAdapter.openBytes(context, bytes)
        try {
            assertEquals(bytes.size.toULong(), adapter.length())
            assertArrayEquals(bytes.copyOfRange(128, 384), adapter.read(128u, 256u))
        } finally {
            adapter.release()
        }
    }

    @Test fun transcodesVariableFrameRateAudioOnTheTrimmedPresentationTimeline() = runBlocking {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val context = instrumentation.targetContext
        val input = File(context.cacheDir, "vfr-aac-input.mp4")
        val output = File(context.cacheDir, "vfr-aac-output.mp4")
        instrumentation.context.assets.open("vfr-edit-list-aac.mp4").use { source -> input.outputStream().use(source::copyTo) }

        val adapter = AndroidMediaAdapter.open(context, Uri.fromFile(input))
        try {
            val report = adapter.renderVideo(VideoRenderRequestRecord(
                output.absolutePath,
                VideoEncodingPlanRecord(128u, 72u, 15u, 400_000u, 96_000u,
                    "video/avc", "audio/mp4a-latm", false),
                TimeRangeRecord(500u, 1_500u),
            ))

            assertTrue(output.length() > 0)
            assertTrue(report.output.durationUs in 750_000u..1_250_000u)
            assertTrue((report.output.frameRate ?: Double.POSITIVE_INFINITY) <= 16.0)
            assertNotNull(report.output.audio)
            assertEquals("audio/mp4a-latm", report.output.audio?.mime)
            assertEquals("video/avc", report.output.videoMime)
        } finally {
            adapter.release()
            input.delete()
            output.delete()
        }
    }
}
