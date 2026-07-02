package com.promtuz.core

import android.content.Context
import com.promtuz.chat.presentation.state.ConnectionState
import com.promtuz.chat.ui.activities.ShareIdentity
import com.promtuz.chat.utils.serialization.AppCbor
import com.promtuz.core.events.EventCallback
import com.promtuz.core.events.InternalEvents
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.decodeFromByteArray
import timber.log.Timber
import java.net.InetAddress

@OptIn(ExperimentalSerializationApi::class)
inline fun <reified T> decode(bytes: ByteArray): T {
    // surgically converting a map into array for compatibility
    // might break things, but works for now
    // blame kotlin serialization api
    if (bytes[0] == 0xA1.toByte()) bytes[0] = 0x82.toByte()
    return AppCbor.instance.decodeFromByteArray<T>(bytes)
}

object API {
    init {
        System.loadLibrary("core")
        Timber.tag("API").d("LOADED LIBCORE");

        registerCallback { bytes ->
            val tagSize = bytes[0]
            val tag = String(bytes.sliceArray(1..tagSize))
            val valueBytes = bytes.copyOfRange(tagSize + 1, bytes.size)

            try {
                val value = when (tag) {
                    "CONNECTION" -> decode<InternalEvents.ConnectionEv>(valueBytes)
                    "IDENTITY" -> decode<InternalEvents.IdentityEv>(valueBytes)
                    "MESSAGE" -> decode<InternalEvents.MessageEv>(valueBytes)
                    else -> error("Unknown InternalEvent $tag")
                }

                _eventsFlow.tryEmit(value)
            } catch (e: Exception) {
                Timber.tag("API").e(e, "ERROR: InternalEvent deserialization failed");
            }
        }
    }

    external fun initApi(context: Context)
    external fun shouldLaunchApp(): Boolean

    //=||=||=||=||=||==|  MISC.  |==||=||=||=||=||=//

    external fun getPublicAddr(): Deferred<InetAddress?>


    //=||=||=||=||=||==|  STATS  |==||=||=||=||=||=//

    // Returns current connection state
    val connectionState: ConnectionState
        get() = ConnectionState.fromInt(getInternalConnectionState())

    private external fun getInternalConnectionState(): Int

    external fun getNetworkStats(): ByteArray

    //=||=||=||=||=||=| CONNECTION |=||=||=||=||=||=//

    external fun connect(context: Context)

    //=||=||=||=||=||==|  EVENTS  |==||=||=||=||=||=//

    private val _eventsFlow = MutableSharedFlow<Any>(
        replay = 0,
        extraBufferCapacity = 64, // Buffer for burst events
        onBufferOverflow = BufferOverflow.DROP_OLDEST
    )

    val eventsFlow: SharedFlow<Any> = _eventsFlow.asSharedFlow()

    private external fun registerCallback(callback: EventCallback)


    //=||=||=||=||=||==| IDENTITY |==||=||=||=||=||=//

    external fun identityInit(identity: ShareIdentity)
    external fun identityAccept()
    external fun identityReject()
    external fun identityDestroy()

    external fun parseQRBytes(bytes: ByteArray)
    external fun computeQrMask(grid: ByteArray, size: Int): ByteArray


    //=||=||=||=||=||==| MESSAGING |==||=||=||=||=||=//

    external fun sendMessage(toIpk: ByteArray, content: String)
    external fun getMessages(peerIpk: ByteArray, limit: Int, beforeId: String?): ByteArray
    external fun getConversations(): ByteArray
    external fun getContacts(): ByteArray


    //=||=||=||=||=||==| WELCOME! |==||=||=||=||=||=//

    external fun welcome(name: String): Boolean
}
