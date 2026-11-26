use horizon_protocol::{
    PROTOCOL_VERSION, State,
    adapter::{ServerVersion, Session},
    buf::{Codec, Writer},
    ids,
    packets::{Empty, configuration, handshake, login},
};
use sha2::{Digest, Sha256};

/// The state-changing subset of an offline join. The adapter needs no transport or world to replay Play.
pub(super) fn play_session(version: ServerVersion) -> horizon_protocol::Result<Session> {
    let session = Session::new(version);
    let intention = handshake::Serverbound::Intention(handshake::Intention {
        protocol: PROTOCOL_VERSION,
        host: "localhost".into(),
        port: 25565,
        intent: handshake::LOGIN,
    });
    assert!(session.serverbound(&intention.encode())?.is_some());
    assert_eq!(
        (session.reading(), session.writing()),
        (State::Login, State::Login)
    );

    let profile = login::GameProfile {
        uuid: 1,
        name: "BenchPlayer".into(),
        properties: Vec::new(),
    };
    let hello = login::Serverbound::Hello(login::ServerHello {
        name: profile.name.clone(),
        uuid: profile.uuid,
    });
    assert!(session.serverbound(&hello.encode())?.is_some());
    let mut success = Writer::new();
    if version == ServerVersion::Current {
        login::LoginFinished::write(
            &login::LoginFinished {
                profile,
                session: 0,
            },
            &mut success,
        );
    } else {
        // 1.20.3 success ends after the profile (no strict-error flag or session UUID).
        login::GameProfile::write(&profile, &mut success);
    }
    let id = version
        .clientbound_id_down(State::Login, ids::login::clientbound::LOGIN_FINISHED)
        .unwrap();
    let (id, payload) = session.clientbound(id, success.data)?.unwrap();
    assert!(matches!(
        login::Clientbound::decode(id, &payload)?,
        login::Clientbound::LoginFinished(_)
    ));
    assert!(
        session
            .serverbound(&login::Serverbound::LoginAcknowledged(Empty).encode())?
            .is_some()
    );
    assert_eq!(
        (session.reading(), session.writing()),
        (State::Configuration, State::Configuration)
    );

    let finish = ids::configuration::clientbound::FINISH_CONFIGURATION;
    let id = version
        .clientbound_id_down(State::Configuration, finish)
        .unwrap();
    let first = session.clientbound(id, Vec::new())?.unwrap();
    // Legacy translation emits synthetic registries ahead of Finish Configuration.
    let (id, payload) = std::iter::successors(Some(first), |_| session.next_incoming())
        .last()
        .unwrap();
    assert_eq!(id, finish);
    assert!(payload.is_empty());
    assert!(
        session
            .serverbound(&configuration::Serverbound::FinishConfiguration(Empty).encode())?
            .is_some()
    );
    assert!(session.take_outgoing().is_empty());
    assert_eq!(
        (session.reading(), session.writing()),
        (State::Play, State::Play)
    );
    Ok(session)
}

/// Feed one length-delimited packet to a per-direction digest. Incoming packets have
/// a separate numeric ID (little-endian i32); outgoing bodies already contain the wire ID.
pub(super) fn hash_packet(digest: &mut Sha256, id: Option<i32>, body: &[u8]) {
    digest.update((body.len() as u64).to_le_bytes());
    if let Some(id) = id {
        digest.update(id.to_le_bytes());
    }
    digest.update(body);
}
