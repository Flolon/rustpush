use std::{rc::Rc, fmt, collections::HashMap, vec, io::Cursor, sync::Arc, str::FromStr};

use openssl::{pkey::PKey, sign::Signer, hash::MessageDigest, encrypt::{Encrypter, Decrypter}, symm::{Cipher, encrypt, decrypt}, rsa::Padding, sha::sha1};
use plist::Data;
use serde::{Serialize, Deserialize};
use tokio::sync::{mpsc::Receiver, Mutex};
use uuid::Uuid;
use rand::Rng;

use crate::{apns::{APNSConnection, APNSPayload}, ids::{user::{IDSUser, IDSIdentityResult}, IDSError, identity::IDSPublicIdentity}, util::{plist_to_bin, gzip, ungzip}};

pub struct BalloonBody {
    pub bid: String,
    pub data: Vec<u8>
}

// represents an IMessage
pub struct IMessage {
    pub text: String,
    pub xml: Option<String>,
    pub participants: Vec<String>,
    pub sender: String,
    pub id: Option<String>,
    pub after_guid: Option<String>,
    pub sender_guid: Option<String>,
    pub body: Option<BalloonBody>,
    pub effect: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RawIMessage {
    #[serde(rename = "t")]
    text: String,
    #[serde(rename = "x")]
    xml: Option<String>,
    #[serde(rename = "p")]
    participants: Vec<String>,
    #[serde(rename = "r")]
    after_guid: Option<String>, // uuid
    #[serde(rename = "gid")]
    sender_guid: Option<String>,
    pv: u64,
    gv: String,
    v: String,
    bid: Option<String>,
    b: Option<Data>,
    #[serde(rename = "iid")]
    effect: Option<String>,
}

impl IMessage {
    fn sanity_check(&mut self) {
        if self.id.is_none() {
            self.id = Some(Uuid::new_v4().to_string());
        }
        if self.sender_guid.is_none() {
            self.sender_guid = Some(Uuid::new_v4().to_string());
        }
        if !self.participants.contains(&self.sender) {
            self.participants.push(self.sender.clone());
        }
    }

    fn to_raw(&mut self) -> Vec<u8> {
        let raw = RawIMessage {
            text: self.text.clone(),
            xml: self.xml.clone(),
            participants: self.participants.clone(),
            after_guid: self.after_guid.clone(),
            sender_guid: self.sender_guid.clone(),
            pv: 0,
            gv: "8".to_string(),
            v: "1".to_string(),
            bid: None,
            b: None,
            effect: self.effect.clone()
        };

        let binary = plist_to_bin(&raw).unwrap();
        
        // do not gzip xml
        let final_msg = if self.xml.is_some() {
            binary
        } else {
            gzip(&binary).unwrap()
        };

        final_msg
    }

    fn from_raw(bytes: &[u8], wrapper: &RecvMsg) -> Option<IMessage> {
        let decompressed = ungzip(&bytes).unwrap_or_else(|_| bytes.to_vec());
        let loaded: RawIMessage = plist::from_bytes(&decompressed).ok()?;
        println!("xml: {:?}", plist::Value::from_reader(Cursor::new(&decompressed)));
        let msg_guid: Vec<u8> = wrapper.msg_guid.clone().into();
        Some(IMessage {
            text: loaded.text.clone(),
            xml: loaded.xml.clone(),
            participants: loaded.participants.clone(),
            sender: wrapper.sender.clone(),
            id: Some(Uuid::from_bytes(msg_guid.try_into().unwrap()).to_string()),
            after_guid: loaded.after_guid.clone(),
            sender_guid: loaded.sender_guid.clone(),
            body: if let Some(body) = &loaded.b {
                if let Some(bid) = &loaded.bid {
                    Some(BalloonBody { bid: bid.clone(), data: body.clone().into() })
                } else { None }
            } else { None },
            effect: loaded.effect.clone()
        })
    }
}

impl fmt::Display for IMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] '{}'", self.sender, self.text)
    }
}

#[derive(Serialize, Deserialize)]
struct BundledPayload {
    #[serde(rename = "tP")]
    participant: String,
    #[serde(rename = "D")]
    not_me: bool,
    #[serde(rename = "sT")]
    session_token: Data,
    #[serde(rename = "P")]
    payload: Data,
    #[serde(rename = "t")]
    token: Data,
}

#[derive(Serialize, Deserialize)]
struct SendMsg {
    fcn: u8,
    c: u8,
    #[serde(rename = "E")]
    e: String,
    ua: String,
    v: u8,
    i: u32,
    #[serde(rename = "U")]
    u: Data,
    dtl: Vec<BundledPayload>,
    #[serde(rename = "sP")]
    sp: String
}

#[derive(Serialize, Deserialize)]
struct RecvMsg {
    #[serde(rename = "P")]
    payload: Data,
    #[serde(rename = "sP")]
    sender: String,
    #[serde(rename = "t")]
    token: Data,
    #[serde(rename = "tP")]
    target: String,
    #[serde(rename = "U")]
    msg_guid: Data,
}


pub struct IMClient {
    pub conn: Arc<APNSConnection>,
    pub users: Arc<Vec<IDSUser>>,
    key_cache: Mutex<HashMap<String, Vec<IDSIdentityResult>>>,
    raw_inbound: Mutex<Receiver<APNSPayload>>,
    pub current_handle: Mutex<String>
}

pub enum RecievedMessage {
    Message {
        msg: IMessage
    }
}

const NORMAL_NONCE: [u8; 16] = [
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1
];

impl IMClient {
    pub async fn new(conn: Arc<APNSConnection>, users: Arc<Vec<IDSUser>>) -> IMClient {
        IMClient {
            key_cache: Mutex::new(HashMap::new()),
            raw_inbound: Mutex::new(conn.reader.register_for(|pay| {
                if pay.id != 0x0A {
                    return false
                }
                if pay.get_field(2).unwrap() != &sha1("com.apple.madrid".as_bytes()) {
                    return false
                }
                let Some(body) = pay.get_field(3) else {
                    return false
                };
                let load = plist::Value::from_reader(Cursor::new(body)).unwrap();
                let has_p = load.as_dictionary().unwrap().contains_key("P");
                has_p
            }).await),
            conn,
            current_handle: Mutex::new(users[0].handles[0].clone()),
            users,
        }
    }

    fn parse_payload(payload: &[u8]) -> (&[u8], &[u8]) {
        let body_len = u16::from_be_bytes(payload[1..3].try_into().unwrap()) as usize;
        let body = &payload[3..(3 + body_len)];
        let sig_len = u8::from_be_bytes(payload[(3 + body_len)..(4 + body_len)].try_into().unwrap()) as usize;
        let sig = &payload[(4 + body_len)..(4 + body_len + sig_len)];
        (body, sig)
    }

    pub async fn use_handle(&self, handle: &str) {
        let mut cache = self.key_cache.lock().await;
        cache.clear();
        let mut current_identity = self.current_handle.lock().await;
        *current_identity = handle.to_string();
    }

    pub fn get_handles(&self) -> Vec<String> {
        self.users.iter().flat_map(|user| user.handles.clone()).collect::<Vec<String>>()
    }

    async fn verify_payload(&self, payload: &[u8], sender: &str, sender_token: &[u8]) -> bool {
        self.cache_keys(&[sender.to_string()]).await.unwrap();

        let cache = self.key_cache.lock().await;
        let Some(keys) = cache.get(sender) else {
            println!("Cannot verify; no public key");
            return false
        };

        let Some(identity) = keys.iter().find(|key| key.push_token == sender_token) else {
            println!("Cannot verify; no public key");
            return false
        };

        let (body, sig) = Self::parse_payload(payload);
        let valid = identity.identity.verify(body, sig).unwrap();

        valid
    }

    pub async fn decrypt(&self, user: &IDSUser, payload: &[u8]) -> Result<Vec<u8>, IDSError> {
        let (body, _sig) = Self::parse_payload(payload);
        
        let key = user.identity.as_ref().unwrap().priv_enc_key();
        let mut decrypter = Decrypter::new(&key)?;
        decrypter.set_rsa_padding(Padding::PKCS1_OAEP)?;
        decrypter.set_rsa_oaep_md(MessageDigest::sha1())?;
        decrypter.set_rsa_mgf1_md(MessageDigest::sha1())?;
        let buffer_len = decrypter.decrypt_len(&payload).unwrap();
        let mut decrypted_asym = vec![0; buffer_len];
        decrypter.decrypt(&body[..160], &mut decrypted_asym[..])?;

        let decrypted_sym = decrypt(Cipher::aes_128_ctr(), &decrypted_asym[..16], Some(&NORMAL_NONCE), &[
            decrypted_asym[16..116].to_vec(),
            body[160..].to_vec()
        ].concat()).unwrap();

        Ok(decrypted_sym)
    }

    pub async fn recieve(&mut self) -> Option<RecievedMessage> {
        let Ok(payload) = self.raw_inbound.lock().await.try_recv() else {
            return None
        };
        self.recieve_payload(payload).await
    }

    pub async fn recieve_wait(&self) -> Option<RecievedMessage> {
        let Some(payload) = self.raw_inbound.lock().await.recv().await else {
            return None
        };
        self.recieve_payload(payload).await
    }

    async fn current_user(&self) -> &IDSUser {
        let current_handle = self.current_handle.lock().await;
        self.users.iter().find(|user| user.handles.contains(&current_handle)).unwrap()
    }

    async fn recieve_payload(&self, payload: APNSPayload) -> Option<RecievedMessage> {
        let body = payload.get_field(3).unwrap();
        let loaded: RecvMsg = plist::from_bytes(body).unwrap();
        println!("xml2: {:?}", plist::Value::from_reader(Cursor::new(&body)));

        let Some(identity) = self.users.iter().find(|user| user.handles.contains(&loaded.target)) else {
            panic!("No identity for sender {}", loaded.sender);
        };

        let payload: Vec<u8> = loaded.payload.clone().into();
        let token: Vec<u8> = loaded.token.clone().into();
        if !self.verify_payload(&payload, &loaded.sender, &token).await {
            panic!("Payload verification failed!");
        }

        let decrypted = self.decrypt(identity, &payload).await.unwrap();
        
        IMessage::from_raw(&decrypted, &loaded).map(|msg| RecievedMessage::Message {
            msg
        })
    }

    pub async fn cache_keys(&self, participants: &[String]) -> Result<(), IDSError> {
        // find participants whose keys need to be fetched
        let key_cache = self.key_cache.lock().await;
        let fetch: Vec<String> = participants.iter().filter(|p| !key_cache.contains_key(*p))
            .map(|p| p.to_string()).collect();
        if fetch.len() == 0 {
            return Ok(())
        }
        drop(key_cache);
        let results = self.current_user().await.lookup(self.conn.clone(), fetch).await?;
        let mut key_cache = self.key_cache.lock().await;
        for (id, results) in results {
            if results.len() == 0 {
                continue // no results
            }
            key_cache.insert(id, results);
        }
        Ok(())
    }

    pub async fn validate_targets(&self, targets: &[String]) -> Result<Vec<String>, IDSError> {
        self.cache_keys(targets).await?;
        let key_cache = self.key_cache.lock().await;
        Ok(targets.iter().filter(|target| key_cache.contains_key(*target)).map(|i| i.clone()).collect())
    }

    pub async fn new_msg(&self, text: &str, targets: &[String], guid: &str) -> IMessage {
        let current_handle = self.current_handle.lock().await;
        IMessage {
            text: text.to_string(),
            xml: None,
            participants: targets.to_vec(),
            sender: current_handle.clone(),
            id: Some(guid.to_string()),
            sender_guid: None,
            body: None,
            effect: None,
            after_guid: None
        }
    }

    async fn encrypt_payload(&self, raw: &[u8], key: &IDSPublicIdentity) -> Result<Vec<u8>, IDSError> {
        let rand = rand::thread_rng().gen::<[u8; 11]>();
        let user = self.current_user().await;

        let hmac = PKey::hmac(&rand)?;
        let mut signer = Signer::new(MessageDigest::sha256(), &hmac)?;
        let result = signer.sign_oneshot_to_vec(&[
            raw.to_vec(),
            vec![0x02],
            user.identity.as_ref().unwrap().public().hash().to_vec(),
            key.hash().to_vec()
        ].concat())?;

        let aes_key = [
            rand.to_vec(),
            result[..5].to_vec()
        ].concat();

        let encrypted_sym = encrypt(Cipher::aes_128_ctr(), &aes_key, Some(&NORMAL_NONCE), raw).unwrap();

        let encryption_key = PKey::from_rsa(key.encryption_key.clone())?;

        let payload = [
            aes_key,
            encrypted_sym[..100].to_vec()
        ].concat();
        let mut encrypter = Encrypter::new(&encryption_key)?;
        encrypter.set_rsa_padding(Padding::PKCS1_OAEP)?;
        encrypter.set_rsa_oaep_md(MessageDigest::sha1())?;
        encrypter.set_rsa_mgf1_md(MessageDigest::sha1())?;
        let buffer_len = encrypter.encrypt_len(&payload).unwrap();
        let mut encrypted = vec![0; buffer_len];
        let encrypted_len = encrypter.encrypt(&payload, &mut encrypted).unwrap();
        encrypted.truncate(encrypted_len);

        let payload = [
            encrypted,
            encrypted_sym[100..].to_vec()
        ].concat();

        let sig = user.identity.as_ref().unwrap().sign(&payload)?;
        let payload = [
            vec![0x02],
            (payload.len() as u16).to_be_bytes().to_vec(),
            payload,
            (sig.len() as u8).to_be_bytes().to_vec(),
            sig
        ].concat();

        Ok(payload)
    }

    pub async fn send(&self, message: &mut IMessage) -> Result<(), IDSError> {
        message.sanity_check();
        self.cache_keys(message.participants.as_ref()).await?;
        let raw = message.to_raw();

        let mut payloads: Vec<BundledPayload> = vec![];

        let key_cache = self.key_cache.lock().await;
        for participant in &message.participants {
            for token in key_cache.get(participant).unwrap() {
                if &token.push_token == self.conn.state.token.as_ref().unwrap() {
                    // don't send to ourself
                    continue;
                }
                let payload = self.encrypt_payload(&raw, &token.identity).await?;
                payloads.push(BundledPayload {
                    participant: participant.clone(),
                    not_me: participant != &message.sender,
                    session_token: token.session_token.clone().into(),
                    payload: payload.into(),
                    token: token.push_token.clone().into()
                });
            }
        }
        drop(key_cache);
        let msg_id = rand::thread_rng().gen::<[u8; 4]>();
        let complete = SendMsg {
            fcn: 1,
            c: 100,
            e: "pair".to_string(),
            ua: "[macOS,13.4.1,22F82,MacBookPro18,3]".to_string(),
            v: 8,
            i: u32::from_be_bytes(msg_id),
            u: Uuid::from_str(message.id.as_ref().unwrap()).unwrap().as_bytes().to_vec().into(),
            dtl: payloads,
            sp: message.sender.clone()
        };

        let binary = plist_to_bin(&complete)?;
        self.conn.send_message("com.apple.madrid", &binary, Some(&msg_id)).await?;

        Ok(())
    }
}