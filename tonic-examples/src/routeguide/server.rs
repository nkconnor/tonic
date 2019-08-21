mod data;

use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;
use tokio::{
    net::TcpListener,
    sync::{mpsc, Lock},
};
use tonic::{Request, Response, Status};
use tower_h2::Server;

pub mod routeguide {
    include!(concat!(env!("OUT_DIR"), "/routeguide.rs"));
}

use routeguide::{Feature, Point, Rectangle, RouteNote, RouteSummary};

#[derive(Debug)]
pub struct RouteGuide {
    state: State,
}

#[derive(Debug, Clone)]
struct State {
    features: Arc<Vec<Feature>>,
    notes: Lock<HashMap<Point, Vec<RouteNote>>>,
}

#[tonic::server(service = "routeguide.RouteGuide", proto = "routeguide")]
impl RouteGuide {
    pub async fn get_feature(&self, request: Request<Point>) -> Result<Response<Feature>, Status> {
        println!("GetFeature = {:?}", request);

        for feature in &self.state.features[..] {
            if feature.location.as_ref() == Some(request.get_ref()) {
                return Ok(Response::new(feature.clone()));
            }
        }

        let response = Response::new(Feature {
            name: "".to_string(),
            location: None,
        });

        Ok(response)
    }

    pub async fn list_features(
        &self,
        request: Request<Rectangle>,
    ) -> Result<Response<mpsc::Receiver<Result<Feature, Status>>>, Status> {
        use std::thread;

        println!("ListFeatures = {:?}", request);

        let (mut tx, rx) = mpsc::channel(4);

        let state = self.state.clone();

        thread::spawn(move || {
            for feature in &state.features[..] {
                if in_range(feature.location.as_ref().unwrap(), request.get_ref()) {
                    println!("  => send {:?}", feature);
                    tx.try_send(Ok(feature.clone())).unwrap();
                }
            }

            println!(" /// done sending");
        });

        Ok(Response::new(rx))
    }

    pub async fn record_route(
        &self,
        request: Request<impl Stream<Item = Result<Point, Status>>>,
    ) -> Result<Response<RouteSummary>, Status> {
        println!("RecordRoute");

        let stream = request.into_inner();

        // Pin the inbound stream to the stack so that we can call next on it
        futures::pin_mut!(stream);

        let mut summary = RouteSummary::default();
        let mut last_point = None;
        let now = Instant::now();

        while let Some(point) = stream.next().await {
            let point = point?;

            println!("  ==> Point = {:?}", point);

            // Increment the point count
            summary.point_count += 1;

            // Find features
            for feature in &self.state.features[..] {
                if feature.location.as_ref() == Some(&point) {
                    summary.feature_count += 1;
                }
            }

            // Calculate the distance
            if let Some(ref last_point) = last_point {
                summary.distance += calc_distance(last_point, &point);
            }

            last_point = Some(point);
        }

        summary.elapsed_time = now.elapsed().as_secs() as i32;

        Ok(Response::new(summary))
    }

    pub async fn route_chat(
        &self,
        request: Request<impl Stream<Item = Result<RouteNote, Status>> + Send + 'static>,
    ) -> Result<Response<impl Stream<Item = Result<RouteNote, Status>> + Send>, Status> {
        println!("RouteChat");

        let stream = request.into_inner();
        let mut state = self.state.clone();

        let output = async_stream::try_stream! {
            futures::pin_mut!(stream);

            while let Some(note) = stream.next().await {
                let note = note?;

                let location = note.location.clone().unwrap();

                let mut notes = state.notes.lock().await;
                let notes = notes.entry(location).or_insert(vec![]);
                notes.push(note);

                for note in notes {
                    yield note.clone();
                }
            }
        };

        Ok(Response::new(output))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:10000".parse().unwrap();
    let mut bind = TcpListener::bind(&addr)?;

    println!("Listening on: {}", bind.local_addr()?);

    let route_guide = RouteGuide {
        state: State {
            // Load data file
            features: Arc::new(data::load()),
            notes: Lock::new(HashMap::new()),
        },
    };
    let mut server = Server::new(RouteGuideServer::new(route_guide), Default::default());

    while let Ok((sock, _addr)) = bind.accept().await {
        if let Err(e) = sock.set_nodelay(true) {
            return Err(e.into());
        }

        if let Err(e) = server.serve(sock).await {
            println!("H2 ERROR: {}", e);
        }
    }

    Ok(())
}

// Implement hash for Point
impl Hash for Point {
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        self.latitude.hash(state);
        self.longitude.hash(state);
    }
}

impl Eq for Point {}

fn in_range(point: &Point, rect: &Rectangle) -> bool {
    use std::cmp;

    let lo = rect.lo.as_ref().unwrap();
    let hi = rect.hi.as_ref().unwrap();

    let left = cmp::min(lo.longitude, hi.longitude);
    let right = cmp::max(lo.longitude, hi.longitude);
    let top = cmp::max(lo.latitude, hi.latitude);
    let bottom = cmp::min(lo.latitude, hi.latitude);

    point.longitude >= left
        && point.longitude <= right
        && point.latitude >= bottom
        && point.latitude <= top
}

/// Calculates the distance between two points using the "haversine" formula.
/// This code was taken from http://www.movable-type.co.uk/scripts/latlong.html.
fn calc_distance(p1: &Point, p2: &Point) -> i32 {
    const CORD_FACTOR: f64 = 1e7;
    const R: f64 = 6371000.0; // meters

    let lat1 = p1.latitude as f64 / CORD_FACTOR;
    let lat2 = p2.latitude as f64 / CORD_FACTOR;
    let lng1 = p1.longitude as f64 / CORD_FACTOR;
    let lng2 = p2.longitude as f64 / CORD_FACTOR;

    let lat_rad1 = lat1.to_radians();
    let lat_rad2 = lat2.to_radians();

    let delta_lat = (lat2 - lat1).to_radians();
    let delta_lng = (lng2 - lng1).to_radians();

    let a = (delta_lat / 2f64).sin() * (delta_lat / 2f64).sin()
        + (lat_rad1).cos() * (lat_rad2).cos() * (delta_lng / 2f64).sin() * (delta_lng / 2f64).sin();

    let c = 2f64 * a.sqrt().atan2((1f64 - a).sqrt());

    (R * c) as i32
}