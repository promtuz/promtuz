package com.promtuz.chat.security

import org.junit.Assert.*
import org.junit.Test

class PrivateSessionPolicyTest {
    @Test fun unexpectedDepartureLocksButOrdinaryAppDoesNot() {
        val policy = PrivateSessionPolicy()
        assertFalse(policy.depart(0))
        policy.arm()
        assertTrue(policy.depart(0))
        assertFalse(policy.armed)
    }
    @Test fun handoffExpiresAtDeadlineEvenIfTimerWasDelayed() {
        val policy = PrivateSessionPolicy()
        policy.arm()
        policy.handoff(100)
        assertFalse(policy.depart(101))
        assertFalse(policy.expire(60_099))
        assertTrue(policy.resume(60_100))
        assertFalse(policy.armed)
    }
    @Test fun returnEndsExceptionAndNextDepartureLocks() {
        val policy = PrivateSessionPolicy()
        policy.arm()
        policy.handoff(0)
        assertFalse(policy.depart(1))
        assertFalse(policy.resume(10))
        assertTrue(policy.depart(11))
    }
    @Test fun screenOffOverridesHandoffAndRepeatedRequestsDoNotExtendIt() {
        val policy = PrivateSessionPolicy()
        policy.arm()
        policy.handoff(0)
        policy.handoff(30_000)
        assertTrue(policy.expire(60_000))
        policy.arm()
        policy.handoff(100_000)
        assertTrue(policy.screenOff())
        assertFalse(policy.screenOff())
    }
    @Test fun recreationAndClearNeverRetainException() {
        val policy = PrivateSessionPolicy()
        policy.arm()
        policy.handoff(0)
        policy.clear()
        policy.arm()
        assertTrue(policy.depart(1))
    }
}
