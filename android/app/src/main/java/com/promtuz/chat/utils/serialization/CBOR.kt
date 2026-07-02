package com.promtuz.chat.utils.serialization

import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.InternalSerializationApi
import kotlinx.serialization.SerializationStrategy
import kotlinx.serialization.cbor.Cbor
import kotlinx.serialization.decodeFromByteArray
import kotlinx.serialization.encodeToByteArray
import kotlinx.serialization.modules.SerializersModule
import kotlinx.serialization.serializer

@OptIn(ExperimentalSerializationApi::class)
inline fun <reified T> cborDecode(bytes: ByteArray): T? {
    return try {
        AppCbor.instance.decodeFromByteArray<T>(bytes)
    } catch (_: Exception) {
        null
    }
}

@OptIn(ExperimentalSerializationApi::class)
object AppCbor {
    @OptIn(InternalSerializationApi::class)
    val instance: Cbor = Cbor {
        ignoreUnknownKeys = true
        encodeDefaults = true
        useDefiniteLengthEncoding = true
        alwaysUseByteString = true
        preferCborLabelsOverNames

    }
}


//@Serializable
//interface CborEnvelope
//
//@OptIn(ExperimentalSerializationApi::class)
//inline fun <reified T> T.toCbor(): ByteArray where T : Any {
//    return AppCbor.instance.encodeToByteArray<T>(this)
//}