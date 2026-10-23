package com.promtuz.chat.utils.media

import android.net.Uri
import android.graphics.Bitmap
import android.media.MediaCodec
import android.media.MediaExtractor
import android.media.MediaMetadataRetriever
import android.media.MediaMuxer
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import java.nio.ByteBuffer
import kotlin.math.abs
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

    @Test fun preservesPictureAndOrientationAcrossSurfaceTranscoding() = runBlocking {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val context = instrumentation.targetContext
        val original = File(context.cacheDir, "orientation-source.mp4")
        instrumentation.context.assets.open("vfr-edit-list-aac.mp4").use { source ->
            original.outputStream().use(source::copyTo)
        }
        try {
            for (rotation in listOf(0, 90, 180, 270)) {
                val input = File(context.cacheDir, "orientation-$rotation-input.mp4")
                val output = File(context.cacheDir, "orientation-$rotation-output.mp4")
                rotateContainer(original, input, rotation)
                val adapter = AndroidMediaAdapter.open(context, Uri.fromFile(input))
                try {
                    val portrait = rotation % 180 != 0
                    adapter.renderVideo(VideoRenderRequestRecord(
                        output.absolutePath,
                        VideoEncodingPlanRecord(
                            if (portrait) 72u else 128u, if (portrait) 128u else 72u,
                            15u, 400_000u, 96_000u, "video/avc", "audio/mp4a-latm", false,
                        ),
                        TimeRangeRecord(500u, 1_500u),
                    ))
                    for (timestamp in listOf(200_000L, 500_000L, 800_000L)) {
                        val expected = framePixels(input, timestamp + 500_000)
                        val actual = framePixels(output, timestamp)
                        val error = expected.indices.sumOf {
                            abs((expected[it].toInt() and 255) - (actual[it].toInt() and 255))
                        }.toDouble() / expected.size
                        assertTrue("rotation=$rotation time=$timestamp pixel error=$error", error < 20)
                    }
                } finally {
                    adapter.release()
                    input.delete()
                    output.delete()
                }
            }
        } finally {
            original.delete()
        }
    }

    private fun framePixels(file: File, timestamp: Long): ByteArray {
        val retriever = MediaMetadataRetriever()
        try {
            retriever.setDataSource(file.absolutePath)
            val frame = requireNotNull(retriever.getFrameAtTime(timestamp, MediaMetadataRetriever.OPTION_CLOSEST))
            val scaled = Bitmap.createScaledBitmap(frame, 32, 32, true)
            return try { scaled.toRgba() } finally { scaled.recycle(); frame.recycle() }
        } finally {
            retriever.release()
        }
    }

    private fun rotateContainer(source: File, output: File, rotation: Int) {
        val extractor = MediaExtractor()
        val muxer = MediaMuxer(output.absolutePath, MediaMuxer.OutputFormat.MUXER_OUTPUT_MPEG_4)
        try {
            extractor.setDataSource(source.absolutePath)
            val tracks = (0 until extractor.trackCount).associateWith {
                extractor.selectTrack(it)
                muxer.addTrack(extractor.getTrackFormat(it))
            }
            muxer.setOrientationHint(rotation)
            muxer.start()
            val buffer = ByteBuffer.allocate(1_048_576)
            val info = MediaCodec.BufferInfo()
            while (extractor.sampleTrackIndex >= 0) {
                buffer.clear()
                val size = extractor.readSampleData(buffer, 0)
                if (size < 0) break
                info.set(0, size, extractor.sampleTime, extractor.sampleFlags)
                muxer.writeSampleData(tracks.getValue(extractor.sampleTrackIndex), buffer, info)
                extractor.advance()
            }
            muxer.stop()
        } finally {
            muxer.release()
            extractor.release()
        }
    }
}
