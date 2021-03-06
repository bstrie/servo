/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use data_loader::decode;
use fetch::cors_cache::{CORSCache, CacheRequestDetails};
use http_loader::{NetworkHttpRequestFactory, create_http_connector, obtain_response};
use hyper::header::{Accept, AcceptLanguage, Authorization, AccessControlAllowCredentials};
use hyper::header::{AccessControlAllowOrigin, AccessControlAllowHeaders, AccessControlAllowMethods};
use hyper::header::{AccessControlRequestHeaders, AccessControlMaxAge, AccessControlRequestMethod, Basic};
use hyper::header::{CacheControl, CacheDirective, ContentEncoding, ContentLength, ContentLanguage, ContentType};
use hyper::header::{Encoding, HeaderView, Headers, IfMatch, IfRange, IfUnmodifiedSince, IfModifiedSince};
use hyper::header::{IfNoneMatch, Pragma, Location, QualityItem, Referer as RefererHeader, UserAgent, q, qitem};
use hyper::method::Method;
use hyper::mime::{Mime, SubLevel, TopLevel};
use hyper::status::StatusCode;
use mime_guess::guess_mime_type;
use net_traits::AsyncFetchListener;
use net_traits::request::{CacheMode, CredentialsMode, Type, Origin, Window};
use net_traits::request::{RedirectMode, Referer, Request, RequestMode, ResponseTainting};
use net_traits::response::{HttpsState, TerminationReason};
use net_traits::response::{Response, ResponseBody, ResponseType};
use resource_thread::CancellationListener;
use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::iter::FromIterator;
use std::rc::Rc;
use std::thread;
use unicase::UniCase;
use url::{Origin as UrlOrigin, Url};
use util::thread::spawn_named;

pub fn fetch_async(request: Request, listener: Box<AsyncFetchListener + Send>) {
    spawn_named(format!("fetch for {:?}", request.current_url_string()), move || {
        let request = Rc::new(request);
        let fetch_response = fetch(request);
        fetch_response.wait_until_done();
        listener.response_available(fetch_response);
    })
}

/// [Fetch](https://fetch.spec.whatwg.org#concept-fetch)
pub fn fetch(request: Rc<Request>) -> Response {
    fetch_with_cors_cache(request, &mut CORSCache::new())
}

pub fn fetch_with_cors_cache(request: Rc<Request>, cache: &mut CORSCache) -> Response {

    // Step 1
    if request.window.get() == Window::Client {
        // TODO: Set window to request's client object if client is a Window object
    } else {
        request.window.set(Window::NoWindow);
    }

    // Step 2
    if *request.origin.borrow() == Origin::Client {
        // TODO: set request's origin to request's client's origin
        unimplemented!()
    }

    // Step 3
    if !request.headers.borrow().has::<Accept>() {

        let value = match request.type_ {

            // Substep 2
            _ if request.is_navigation_request() =>
                vec![qitem(mime!(Text / Html)),
                     // FIXME: This should properly generate a MimeType that has a
                     // SubLevel of xhtml+xml (https://github.com/hyperium/mime.rs/issues/22)
                     qitem(mime!(Application / ("xhtml+xml") )),
                     QualityItem::new(mime!(Application / Xml), q(0.9)),
                     QualityItem::new(mime!(_ / _), q(0.8))],

            // Substep 3
            Type::Image =>
                vec![qitem(mime!(Image / Png)),
                     // FIXME: This should properly generate a MimeType that has a
                     // SubLevel of svg+xml (https://github.com/hyperium/mime.rs/issues/22)
                     qitem(mime!(Image / ("svg+xml") )),
                     QualityItem::new(mime!(Image / _), q(0.8)),
                     QualityItem::new(mime!(_ / _), q(0.5))],

            // Substep 3
            Type::Style =>
                vec![qitem(mime!(Text / Css)),
                     QualityItem::new(mime!(_ / _), q(0.1))],
            // Substep 1
            _ => vec![qitem(mime!(_ / _))]
        };

        // Substep 4
        request.headers.borrow_mut().set(Accept(value));
    }

    // Step 4
    if !request.headers.borrow().has::<AcceptLanguage>() {
        request.headers.borrow_mut().set(AcceptLanguage(vec![qitem("en-US".parse().unwrap())]));
    }

    // Step 5
    // TODO: Figure out what a Priority object is

    // Step 6
    if request.is_subresource_request() {
        // TODO: create a fetch record and append it to request's client's fetch group list
    }
    // Step 7
    main_fetch(request, cache, false, false)
}

/// [Main fetch](https://fetch.spec.whatwg.org/#concept-main-fetch)
fn main_fetch(request: Rc<Request>, cache: &mut CORSCache, cors_flag: bool, recursive_flag: bool) -> Response {
    // TODO: Implement main fetch spec

    // Step 1
    let mut response = None;

    // Step 2
    if request.local_urls_only {
        match request.current_url().scheme() {
            "about" | "blob" | "data" | "filesystem" => (), // Ok, the URL is local.
            _ => response = Some(Response::network_error())
        }
    }

    // Step 3
    // TODO be able to execute report CSP

    // Step 4
    // TODO this step, based off of http_loader.rs

    // Step 5
    // TODO this step

    // Step 6
    if request.referer != Referer::NoReferer {
        // TODO be able to invoke "determine request's referer"
    }

    // Step 7
    // TODO this step

    // Step 8
    // this step is obsoleted by fetch_async

    // Step 9
    let response = match response {
        Some(response) => response,
        None => {
            let current_url = request.current_url();
            let same_origin = if let Origin::Origin(ref origin) = *request.origin.borrow() {
                *origin == current_url.origin()
            } else {
                false
            };

            if (same_origin && !cors_flag ) ||
                (current_url.scheme() == "data" && request.same_origin_data.get()) ||
                (current_url.scheme() == "file" && request.same_origin_data.get()) ||
                current_url.scheme() == "about" ||
                request.mode == RequestMode::Navigate {

                basic_fetch(request.clone(), cache)

            } else if request.mode == RequestMode::SameOrigin {
                Response::network_error()

            } else if request.mode == RequestMode::NoCORS {
                request.response_tainting.set(ResponseTainting::Opaque);
                basic_fetch(request.clone(), cache)

            } else if !matches!(current_url.scheme(), "http" | "https") {
                Response::network_error()

            } else if request.use_cors_preflight ||
                (request.unsafe_request &&
                 (!is_simple_method(&request.method.borrow()) ||
                  request.headers.borrow().iter().any(|h| !is_simple_header(&h)))) {

                request.response_tainting.set(ResponseTainting::CORSTainting);
                request.redirect_mode.set(RedirectMode::Error);
                let response = http_fetch(request.clone(), cache, true, true, false);
                if response.is_network_error() {
                    // TODO clear cache entries using request
                }
                response

            } else {
                request.response_tainting.set(ResponseTainting::CORSTainting);
                http_fetch(request.clone(), cache, true, false, false)
            }
        }
    };

    // Step 10
    if recursive_flag {
        return response;
    }

    // Step 11
    // no need to check if response is a network error, since the type would not be `Default`
    let response = if response.response_type == ResponseType::Default {
        let response_type = match request.response_tainting.get() {
            ResponseTainting::Basic => ResponseType::Basic,
            ResponseTainting::CORSTainting => ResponseType::CORS,
            ResponseTainting::Opaque => ResponseType::Opaque,
        };
        response.to_filtered(response_type)
    } else {
        response
    };

    {
        // Step 12
        let network_error_res = Response::network_error();
        let internal_response = if response.is_network_error() {
            &network_error_res
        } else {
            response.actual_response()
        };

        // Step 13
        // TODO this step

        // Step 14
        if !response.is_network_error() && (is_null_body_status(&internal_response.status) ||
            match *request.method.borrow() {
                Method::Head | Method::Connect => true,
                _ => false })
            {
            // when Fetch is used only asynchronously, we will need to make sure
            // that nothing tries to write to the body at this point
            let mut body = internal_response.body.lock().unwrap();
            *body = ResponseBody::Empty;
        }

        // Step 15
        // TODO be able to compare response integrity against request integrity metadata
        // if !response.is_network_error() {

        //     // Substep 1
        //     response.wait_until_done();

        //     // Substep 2
        //     if response.termination_reason.is_none() {
        //         response = Response::network_error();
        //         internal_response = Response::network_error();
        //     }
        // }
    }

    // Step 16
    if request.synchronous {
        response.actual_response().wait_until_done();
        return response;
    }

    // Step 17
    if request.body.borrow().is_some() && matches!(request.current_url().scheme(), "http" | "https") {
        // TODO queue a fetch task on request to process end-of-file
    }

    {
        // Step 12 repeated to use internal_response
        let network_error_res = Response::network_error();
        let internal_response = if response.is_network_error() {
            &network_error_res
        } else {
            response.actual_response()
        };

        // Step 18
        // TODO this step

        // Step 19
        internal_response.wait_until_done();

        // Step 20
        // TODO this step
    }

    // TODO remove this line when only asynchronous fetches are used
    return response;
}

/// [Basic fetch](https://fetch.spec.whatwg.org#basic-fetch)
fn basic_fetch(request: Rc<Request>, cache: &mut CORSCache) -> Response {

    let url = request.current_url();

    match url.scheme() {

        "about" if url.path() == "blank" => {
            let mut response = Response::new();
            response.headers.set(ContentType(mime!(Text / Html; Charset = Utf8)));
            *response.body.lock().unwrap() = ResponseBody::Done(vec![]);
            response
        },

        "http" | "https" => {
            http_fetch(request.clone(), cache, false, false, false)
        },

        "data" => {
            if *request.method.borrow() == Method::Get {
                match decode(&url) {
                    Ok((mime, bytes)) => {
                        let mut response = Response::new();
                        *response.body.lock().unwrap() = ResponseBody::Done(bytes);
                        response.headers.set(ContentType(mime));
                        response
                    },
                    Err(_) => Response::network_error()
                }
            } else {
                Response::network_error()
            }
        },

        "file" => {
            if *request.method.borrow() == Method::Get {
                match url.to_file_path() {
                    Ok(file_path) => {
                        File::open(file_path.clone()).ok().map_or(Response::network_error(), |mut file| {
                            let mut bytes = vec![];
                            let _ = file.read_to_end(&mut bytes);
                            let mime = guess_mime_type(file_path);

                            let mut response = Response::new();
                            *response.body.lock().unwrap() = ResponseBody::Done(bytes);
                            response.headers.set(ContentType(mime));
                            response
                        })
                    },
                    _ => Response::network_error()
                }
            } else {
                Response::network_error()
            }
        },

        "blob" | "ftp" => {
            // XXXManishearth handle these
            panic!("Unimplemented scheme for Fetch")
        },

        _ => Response::network_error()
    }
}

/// [HTTP fetch](https://fetch.spec.whatwg.org#http-fetch)
fn http_fetch(request: Rc<Request>,
              cache: &mut CORSCache,
              cors_flag: bool,
              cors_preflight_flag: bool,
              authentication_fetch_flag: bool) -> Response {

    // Step 1
    let mut response: Option<Response> = None;

    // Step 2
    // nothing to do, since actual_response is a function on response

    // Step 3
    if !request.skip_service_worker.get() && !request.is_service_worker_global_scope {

        // Substep 1
        // TODO (handle fetch unimplemented)

        if let Some(ref res) = response {

            // Substep 2
            // nothing to do, since actual_response is a function on response

            // Substep 3
            if (res.response_type == ResponseType::Opaque &&
                request.mode != RequestMode::NoCORS) ||
               (res.response_type == ResponseType::OpaqueRedirect &&
                request.redirect_mode.get() != RedirectMode::Manual) ||
               (res.url_list.borrow().len() > 1 &&
                request.redirect_mode.get() != RedirectMode::Follow) ||
               res.response_type == ResponseType::Error {
                return Response::network_error();
            }

            // Substep 4
            let actual_response = res.actual_response();
            if actual_response.url_list.borrow().is_empty() {
                *actual_response.url_list.borrow_mut() = request.url_list.borrow().clone();
            }

            // Substep 5
            // TODO: set response's CSP list on actual_response
        }
    }

    // Step 4
    if response.is_none() {

        // Substep 1
        if cors_preflight_flag {
            let origin = request.origin.borrow().clone();
            let url = request.current_url();
            let credentials = request.credentials_mode == CredentialsMode::Include;
            let method_cache_match = cache.match_method(CacheRequestDetails {
                origin: origin.clone(),
                destination: url.clone(),
                credentials: credentials
            }, request.method.borrow().clone());

            let method_mismatch = !method_cache_match && (!is_simple_method(&request.method.borrow()) ||
                                                          request.use_cors_preflight);
            let header_mismatch = request.headers.borrow().iter().any(|view|
                !cache.match_header(CacheRequestDetails {
                    origin: origin.clone(),
                    destination: url.clone(),
                    credentials: credentials
                }, view.name()) && !is_simple_header(&view)
            );

            // Sub-substep 1
            if method_mismatch || header_mismatch {
                let preflight_result = cors_preflight_fetch(request.clone(), cache);
                // Sub-substep 2
                if preflight_result.response_type == ResponseType::Error {
                    return Response::network_error();
                }
            }
        }

        // Substep 2
        request.skip_service_worker.set(true);

        // Substep 3
        let credentials = match request.credentials_mode {
            CredentialsMode::Include => true,
            CredentialsMode::CredentialsSameOrigin if request.response_tainting.get() == ResponseTainting::Basic
                => true,
            _ => false
        };

        // Substep 4
        let fetch_result = http_network_or_cache_fetch(request.clone(), credentials, authentication_fetch_flag);

        // Substep 5
        if cors_flag && cors_check(request.clone(), &fetch_result).is_err() {
            return Response::network_error();
        }

        fetch_result.return_internal.set(false);
        response = Some(fetch_result);
    }

    // response is guaranteed to be something by now
    let mut response = response.unwrap();

    // Step 5
    match response.actual_response().status.unwrap() {

        // Code 301, 302, 303, 307, 308
        StatusCode::MovedPermanently | StatusCode::Found | StatusCode::SeeOther |
        StatusCode::TemporaryRedirect | StatusCode::PermanentRedirect => {

            response = match request.redirect_mode.get() {
                RedirectMode::Error => Response::network_error(),
                RedirectMode::Manual => {
                    response.to_filtered(ResponseType::OpaqueRedirect)
                },
                RedirectMode::Follow => {
                    // set back to default
                    response.return_internal.set(true);
                    http_redirect_fetch(request, cache, Rc::new(response), cors_flag)
                }
            }
        },

        // Code 401
        StatusCode::Unauthorized => {

            // Step 1
            // FIXME: Figure out what to do with request window objects
            if cors_flag || request.credentials_mode != CredentialsMode::Include {
                return response;
            }

            // Step 2
            // TODO: Spec says requires testing on multiple WWW-Authenticate headers

            // Step 3
            if !request.use_url_credentials || authentication_fetch_flag {
                // TODO: Prompt the user for username and password from the window
            }

            // Step 4
            return http_fetch(request, cache, cors_flag, cors_preflight_flag, true);
        }

        // Code 407
        StatusCode::ProxyAuthenticationRequired => {

            // Step 1
            // TODO: Figure out what to do with request window objects

            // Step 2
            // TODO: Spec says requires testing on Proxy-Authenticate headers

            // Step 3
            // TODO: Prompt the user for proxy authentication credentials

            // Step 4
            return http_fetch(request, cache,
                              cors_flag, cors_preflight_flag,
                              authentication_fetch_flag);
        }

        _ => { }
    }

    // Step 6
    if authentication_fetch_flag {
        // TODO: Create authentication entry for this request
    }

    // set back to default
    response.return_internal.set(true);
    // Step 7
    response
}

/// [HTTP redirect fetch](https://fetch.spec.whatwg.org#http-redirect-fetch)
fn http_redirect_fetch(request: Rc<Request>,
                       cache: &mut CORSCache,
                       response: Rc<Response>,
                       cors_flag: bool) -> Response {

    // Step 1
    assert_eq!(response.return_internal.get(), true);

    // Step 3
    // this step is done early, because querying if Location exists says
    // if it is None or Some, making it easy to seperate from the retrieval failure case
    if !response.actual_response().headers.has::<Location>() {
        return Rc::try_unwrap(response).ok().unwrap();
    }

    // Step 2
    let location = match response.actual_response().headers.get::<Location>() {
        Some(&Location(ref location)) => location.clone(),
        // Step 4
        _ => return Response::network_error()
    };

    // Step 5
    let response_url = response.actual_response().url.as_ref().unwrap();
    let location_url = response_url.join(&*location);

    // Step 6
    let location_url = match location_url {
        Ok(url) => url,
        _ => return Response::network_error()
    };

    // Step 7
    if request.redirect_count.get() >= 20 {
        return Response::network_error();
    }

    // Step 8
    request.redirect_count.set(request.redirect_count.get() + 1);

    // Step 9
    request.same_origin_data.set(false);

    let same_origin = if let Origin::Origin(ref origin) = *request.origin.borrow() {
        *origin == request.current_url().origin()
    } else {
        false
    };
    let has_credentials = has_credentials(&location_url);

    // Step 10
    if request.mode == RequestMode::CORSMode && !same_origin && has_credentials {
        return Response::network_error();
    }

    // Step 11
    if cors_flag && has_credentials {
        return Response::network_error();
    }

    // Step 12
    if cors_flag && !same_origin {
        *request.origin.borrow_mut() = Origin::Origin(UrlOrigin::new_opaque());
    }

    // Step 13
    let status_code = response.actual_response().status.unwrap();
    if ((status_code == StatusCode::MovedPermanently || status_code == StatusCode::Found) &&
        *request.method.borrow() == Method::Post) ||
        status_code == StatusCode::SeeOther {

        *request.method.borrow_mut() = Method::Get;
        *request.body.borrow_mut() = None;
    }

    // Step 14
    request.url_list.borrow_mut().push(location_url);

    // Step 15
    main_fetch(request, cache, cors_flag, true)
}

/// [HTTP network or cache fetch](https://fetch.spec.whatwg.org#http-network-or-cache-fetch)
fn http_network_or_cache_fetch(request: Rc<Request>,
                               credentials_flag: bool,
                               authentication_fetch_flag: bool) -> Response {

    // TODO: Implement Window enum for Request
    let request_has_no_window = true;

    // Step 1
    let http_request = if request_has_no_window &&
        request.redirect_mode.get() != RedirectMode::Follow {
        request.clone()
    } else {
        Rc::new((*request).clone())
    };

    let content_length_value = match *http_request.body.borrow() {
        None =>
            match *http_request.method.borrow() {
                // Step 3
                Method::Head | Method::Post | Method::Put =>
                    Some(0),
                // Step 2
                _ => None
            },
        // Step 4
        Some(ref http_request_body) => Some(http_request_body.len() as u64)
    };

    // Step 5
    if let Some(content_length_value) = content_length_value {
        http_request.headers.borrow_mut().set(ContentLength(content_length_value));
    }

    // Step 6
    match http_request.referer {
        Referer::NoReferer =>
            http_request.headers.borrow_mut().set(RefererHeader("".to_owned())),
        Referer::RefererUrl(ref http_request_referer) =>
            http_request.headers.borrow_mut().set(RefererHeader(http_request_referer.to_string())),
        Referer::Client =>
            // it should be impossible for referer to be anything else during fetching
            // https://fetch.spec.whatwg.org/#concept-request-referrer
            unreachable!()
    };

    // Step 7
    if http_request.omit_origin_header.get() == false {
        // TODO update this when https://github.com/hyperium/hyper/pull/691 is finished
        // http_request.headers.borrow_mut().set_raw("origin", origin);
    }

    // Step 8
    if !http_request.headers.borrow().has::<UserAgent>() {
        http_request.headers.borrow_mut().set(UserAgent(global_user_agent().to_owned()));
    }

    match http_request.cache_mode.get() {

        // Step 9
        CacheMode::Default if is_no_store_cache(&http_request.headers.borrow()) => {
            http_request.cache_mode.set(CacheMode::NoStore);
        },

        // Step 10
        CacheMode::NoCache if !http_request.headers.borrow().has::<CacheControl>() => {
            http_request.headers.borrow_mut().set(CacheControl(vec![CacheDirective::MaxAge(0)]));
        },

        // Step 11
        CacheMode::Reload => {
            // Substep 1
            if !http_request.headers.borrow().has::<Pragma>() {
                http_request.headers.borrow_mut().set(Pragma::NoCache);
            }

            // Substep 2
            if !http_request.headers.borrow().has::<CacheControl>() {
                http_request.headers.borrow_mut().set(CacheControl(vec![CacheDirective::NoCache]));
            }
        },

        _ => {}
    }

    // Step 12
    // modify_request_headers(http_request.headers.borrow());

    // Step 13
    // TODO some of this step can't be implemented yet
    if credentials_flag {

        // Substep 1
        // TODO http://mxr.mozilla.org/servo/source/components/net/http_loader.rs#504

        // Substep 2
        if !http_request.headers.borrow().has::<Authorization<String>>() {

            // Substep 3
            let mut authorization_value = None;

            // Substep 4
            // TODO be able to retrieve https://fetch.spec.whatwg.org/#authentication-entry

            // Substep 5
            if authentication_fetch_flag {

                let current_url = http_request.current_url();

                authorization_value = if has_credentials(&current_url) {
                    Some(Basic {
                        username: current_url.username().to_owned(),
                        password: current_url.password().map(str::to_owned)
                    })
                } else {
                    None
                }
            }

            // Substep 6
            if let Some(basic) = authorization_value {
                http_request.headers.borrow_mut().set(Authorization(basic));
            }
        }
    }

    // Step 14
    // TODO this step can't be implemented yet

    // Step 15
    let mut response: Option<Response> = None;

    // Step 16
    // TODO have a HTTP cache to check for a completed response
    let complete_http_response_from_cache: Option<Response> = None;
    if http_request.cache_mode.get() != CacheMode::NoStore &&
        http_request.cache_mode.get() != CacheMode::Reload &&
        complete_http_response_from_cache.is_some() {

        // Substep 1
        if http_request.cache_mode.get() == CacheMode::ForceCache {
            // TODO pull response from HTTP cache
            // response = http_request
        }

        let revalidation_needed = match response {
            Some(ref response) => response_needs_revalidation(&response),
            _ => false
        };

        // Substep 2
        if !revalidation_needed && http_request.cache_mode.get() == CacheMode::Default {
            // TODO pull response from HTTP cache
            // response = http_request
            // response.cache_state = CacheState::Local;
        }

        // Substep 3
        if revalidation_needed && http_request.cache_mode.get() == CacheMode::Default ||
            http_request.cache_mode.get() == CacheMode::NoCache {

            // TODO this substep
        }

    // Step 17
    // TODO have a HTTP cache to check for a partial response
    } else if http_request.cache_mode.get() == CacheMode::Default ||
        http_request.cache_mode.get() == CacheMode::ForceCache {
        // TODO this substep
    }

    // Step 18
    if response.is_none() {
        response = Some(http_network_fetch(request.clone(), http_request.clone(), credentials_flag));
    }
    let response = response.unwrap();

    // Step 19
    if let Some(status) = response.status {
        if status == StatusCode::NotModified &&
            (http_request.cache_mode.get() == CacheMode::Default ||
            http_request.cache_mode.get() == CacheMode::NoCache) {

            // Substep 1
            // TODO this substep
            // let cached_response: Option<Response> = None;

            // Substep 2
            // if cached_response.is_none() {
            //     return Response::network_error();
            // }

            // Substep 3

            // Substep 4
            // response = cached_response;

            // Substep 5
            // TODO cache_state is immutable?
            // response.cache_state = CacheState::Validated;
        }
    }

    // Step 20
    response
}

/// [HTTP network fetch](https://fetch.spec.whatwg.org/#http-network-fetch)
fn http_network_fetch(request: Rc<Request>,
                      _http_request: Rc<Request>,
                      _credentials_flag: bool) -> Response {
    // TODO: Implement HTTP network fetch spec

    // Step 1
    // nothing to do here, since credentials_flag is already a boolean

    // Step 2
    // TODO be able to create connection using current url's origin and credentials
    let connection = create_http_connector();

    // Step 3
    // TODO be able to tell if the connection is a failure

    // Step 4
    let factory = NetworkHttpRequestFactory {
        connector: connection,
    };
    let url = request.current_url();
    let cancellation_listener = CancellationListener::new(None);

    let wrapped_response = obtain_response(&factory, &url, &request.method.borrow(),
                                           &request.headers.borrow(),
                                           &cancellation_listener, &None, &request.method.borrow(),
                                           &None, request.redirect_count.get(), &None, "");

    let mut response = Response::new();
    match wrapped_response {
        Ok(mut res) => {
            response.url = Some(res.response.url.clone());
            response.status = Some(res.response.status);
            response.headers = res.response.headers.clone();

            let res_body = response.body.clone();
            thread::spawn(move || {

                *res_body.lock().unwrap() = ResponseBody::Receiving(vec![]);
                let mut new_body = vec![];
                res.response.read_to_end(&mut new_body).unwrap();

                let mut body = res_body.lock().unwrap();
                assert!(*body != ResponseBody::Empty);
                *body = ResponseBody::Done(new_body);

                // TODO: the vec storage format is much too slow for these operations,
                // response.body needs to use something else before this code can be used
                // *res_body.lock().unwrap() = ResponseBody::Receiving(vec![]);

                // loop {
                //     match read_block(&mut res.response) {
                //         Ok(ReadResult::Payload(ref mut new_body)) => {
                //             if let ResponseBody::Receiving(ref mut body) = *res_body.lock().unwrap() {
                //                 (body).append(new_body);
                //             }
                //         },
                //         Ok(ReadResult::EOF) | Err(_) => break
                //     }

                // }

                // let mut completed_body = res_body.lock().unwrap();
                // if let ResponseBody::Receiving(ref body) = *completed_body {
                //     // TODO cloning seems sub-optimal, but I couldn't figure anything else out
                //     *res_body.lock().unwrap() = ResponseBody::Done((*body).clone());
                // }
            });
        },
        Err(_) =>
            response.termination_reason = Some(TerminationReason::Fatal)
    };

        // TODO these substeps aren't possible yet
        // Substep 1

        // Substep 2

    // TODO Determine if response was retrieved over HTTPS
    // TODO Servo needs to decide what ciphers are to be treated as "deprecated"
    response.https_state = HttpsState::None;

    // TODO Read request

    // Step 5
    // TODO when https://bugzilla.mozilla.org/show_bug.cgi?id=1030660
    // is resolved, this step will become uneccesary
    // TODO this step
    if let Some(encoding) = response.headers.get::<ContentEncoding>() {
        if encoding.contains(&Encoding::Gzip) {

        }

        else if encoding.contains(&Encoding::Compress) {

        }
    };

    // Step 6
    *response.url_list.borrow_mut() = request.url_list.borrow().clone();

    // Step 7
    // TODO this step isn't possible yet

    // Step 8
    if response.is_network_error() && request.cache_mode.get() == CacheMode::NoStore {
        // TODO update response in the HTTP cache for request
    }

    // TODO this step isn't possible yet
    // Step 9

    // TODO these steps
    // Step 10
        // Substep 1
        // Substep 2
            // Sub-substep 1
            // Sub-substep 2
            // Sub-substep 3
            // Sub-substep 4
        // Substep 3

    // Step 11
    response
}

/// [CORS preflight fetch](https://fetch.spec.whatwg.org#cors-preflight-fetch)
fn cors_preflight_fetch(request: Rc<Request>, cache: &mut CORSCache) -> Response {
    // Step 1
    let mut preflight = Request::new(request.current_url(), Some(request.origin.borrow().clone()), false);
    *preflight.method.borrow_mut() = Method::Options;
    preflight.initiator = request.initiator.clone();
    preflight.type_ = request.type_.clone();
    preflight.destination = request.destination.clone();
    preflight.referer = request.referer.clone();

    // Step 2
    preflight.headers.borrow_mut().set::<AccessControlRequestMethod>(
        AccessControlRequestMethod(request.method.borrow().clone()));

    // Step 3, 4
    let mut value = request.headers.borrow().iter()
                                            .filter_map(|ref view| if is_simple_header(view) {
                                                None
                                            } else {
                                                Some(UniCase(view.name().to_owned()))
                                            }).collect::<Vec<UniCase<String>>>();
    value.sort();

    // Step 5
    preflight.headers.borrow_mut().set::<AccessControlRequestHeaders>(
        AccessControlRequestHeaders(value));

    // Step 6
    let preflight = Rc::new(preflight);
    let response = http_network_or_cache_fetch(preflight.clone(), false, false);

    // Step 7
    if cors_check(request.clone(), &response).is_ok() &&
       response.status.map_or(false, |status| status.is_success()) {
        // Substep 1
        let mut methods = if response.headers.has::<AccessControlAllowMethods>() {
            match response.headers.get::<AccessControlAllowMethods>() {
                Some(&AccessControlAllowMethods(ref m)) => m.clone(),
                // Substep 3
                None => return Response::network_error()
            }
        } else {
            vec![]
        };

        // Substep 2
        let header_names = if response.headers.has::<AccessControlAllowHeaders>() {
            match response.headers.get::<AccessControlAllowHeaders>() {
                Some(&AccessControlAllowHeaders(ref hn)) => hn.clone(),
                // Substep 3
                None => return Response::network_error()
            }
        } else {
            vec![]
        };

        // Substep 4
        if methods.is_empty() && request.use_cors_preflight {
            methods = vec![request.method.borrow().clone()];
        }

        // Substep 5
        if methods.iter().all(|method| *method != *request.method.borrow()) &&
            !is_simple_method(&*request.method.borrow()) {
            return Response::network_error();
        }

        // Substep 6
        let set: HashSet<&UniCase<String>> = HashSet::from_iter(header_names.iter());
        if request.headers.borrow().iter().any(|ref hv| !set.contains(&UniCase(hv.name().to_owned())) &&
                                                        !is_simple_header(hv)) {
            return Response::network_error();
        }

        // Substep 7, 8
        let max_age = response.headers.get::<AccessControlMaxAge>().map(|acma| acma.0).unwrap_or(0);

        // TODO: Substep 9 - Need to define what an imposed limit on max-age is

        // Substep 11, 12
        for method in &methods {
            cache.match_method_and_update(CacheRequestDetails {
                origin: request.origin.borrow().clone(),
                destination: request.current_url(),
                credentials: request.credentials_mode == CredentialsMode::Include
            }, method.clone(), max_age);
        }

        // Substep 13, 14
        for header_name in &header_names {
            cache.match_header_and_update(CacheRequestDetails {
                origin: request.origin.borrow().clone(),
                destination: request.current_url(),
                credentials: request.credentials_mode == CredentialsMode::Include
            }, &*header_name, max_age);
        }

        // Substep 15
        return response;
    }

    // Step 8
    Response::network_error()
}

/// [CORS check](https://fetch.spec.whatwg.org#concept-cors-check)
fn cors_check(request: Rc<Request>, response: &Response) -> Result<(), ()> {

    // Step 1
    let origin = response.headers.get::<AccessControlAllowOrigin>().cloned();

    // Step 2
    let origin = try!(origin.ok_or(()));

    // Step 3
    if request.credentials_mode != CredentialsMode::Include &&
       origin == AccessControlAllowOrigin::Any {
        return Ok(());
    }

    // Step 4
    let origin = match origin {
        AccessControlAllowOrigin::Value(origin) => origin,
        // if it's Any or Null at this point, there's nothing to do but return Err(())
        _ => return Err(())
    };

    match *request.origin.borrow() {
        Origin::Origin(ref o) if o.ascii_serialization() == origin => {},
        _ => return Err(())
    }

    // Step 5
    if request.credentials_mode != CredentialsMode::Include {
        return Ok(());
    }

    // Step 6
    let credentials = request.headers.borrow().get::<AccessControlAllowCredentials>().cloned();

    // Step 7
    if credentials.is_some() {
        return Ok(());
    }

    // Step 8
    Err(())
}

fn global_user_agent() -> String {
    // TODO have a better useragent string
    const USER_AGENT_STRING: &'static str = "Servo";
    USER_AGENT_STRING.to_owned()
}

fn has_credentials(url: &Url) -> bool {
    !url.username().is_empty() || url.password().is_some()
}

fn is_no_store_cache(headers: &Headers) -> bool {
    headers.has::<IfModifiedSince>() | headers.has::<IfNoneMatch>() |
    headers.has::<IfUnmodifiedSince>() | headers.has::<IfMatch>() |
    headers.has::<IfRange>()
}

fn is_simple_header(h: &HeaderView) -> bool {
    if h.is::<ContentType>() {
        match h.value() {
            Some(&ContentType(Mime(TopLevel::Text, SubLevel::Plain, _))) |
            Some(&ContentType(Mime(TopLevel::Application, SubLevel::WwwFormUrlEncoded, _))) |
            Some(&ContentType(Mime(TopLevel::Multipart, SubLevel::FormData, _))) => true,
            _ => false

        }
    } else {
        h.is::<Accept>() || h.is::<AcceptLanguage>() || h.is::<ContentLanguage>()
    }
}

fn is_simple_method(m: &Method) -> bool {
    match *m {
        Method::Get | Method::Head | Method::Post => true,
        _ => false
    }
}

fn response_needs_revalidation(_response: &Response) -> bool {
    // TODO this function
    false
}

// fn modify_request_headers(headers: &mut Headers) -> {
//     // TODO this function

// }

fn is_null_body_status(status: &Option<StatusCode>) -> bool {
    match *status {
        Some(status) => match status {
            StatusCode::SwitchingProtocols | StatusCode::NoContent |
                StatusCode::ResetContent | StatusCode::NotModified => true,
            _ => false
        },
        _ => false
    }
}
